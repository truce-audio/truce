/**
 * Truce AU v2 shim.
 *
 * Implements AudioComponentPlugInInterface for AU v2 hosts (Reaper, auval).
 * The factory function returns this interface. All plugin logic is delegated
 * to Rust via the shared AuCallbacks table.
 */

#include <AudioToolbox/AudioToolbox.h>
#include <CoreFoundation/CoreFoundation.h>
#include <CoreMIDI/CoreMIDI.h>
#include <string.h>
#include <stdlib.h>
#include <dlfcn.h>

#include "au_shim_types.h"

// ---------------------------------------------------------------------------
// Per-instance state
// ---------------------------------------------------------------------------

typedef struct {
    AudioComponentPlugInInterface interface; // MUST be first
    AudioComponentInstance componentInstance;
    void *rustCtx;

    AudioStreamBasicDescription inputFormat;
    AudioStreamBasicDescription outputFormat;
    double sampleRate;
    UInt32 maxFramesPerSlice;
    Boolean initialized;

    // kAudioUnitProperty_PresentPreset (= 36) state. The host writes
    // an AUPreset (presetNumber + retained CFStringRef name) here and
    // reads it back; we also round-trip the name through
    // kAudioUnitProperty_ClassInfo's `kAUPresetNameKey` (= "name") so
    // auval's "Preset name is not retained in retrieved class data"
    // check passes. Negative presetNumber means "user preset, not from
    // the factory list" per Apple convention.
    AUPreset currentPreset;

    // Internal output buffers
    float *outputBuffers[32];
    UInt32 outputBufferSize; // in frames

    // Input callback (for effects)
    AURenderCallback inputCallback;
    void *inputCallbackRefCon;

    // Connection-based input
    AudioUnit sourceUnit;
    UInt32 sourceOutputBus;

    // MIDI buffer (for instruments)
    AuMidiEvent midiBuffer[256];
    uint32_t midiCount;

    // Plugin → host MIDI output callback (set via
    // kAudioUnitProperty_MIDIOutputCallback). Hosts that want to
    // receive MIDI from instruments register the callback once after
    // initialization; we drain `output_events` from Rust at the end
    // of each render block and forward via this callback.
    AUMIDIOutputCallback midiOutputCallback;
    void *midiOutputUserData;

    // Heap-allocated scratch for SysEx output packet lists. Sized
    // to hold one MIDIPacketList of up to 256 SysEx events whose
    // total framed payload tops out at truce_core::SYSEX_POOL_PREALLOC
    // (128 KiB) + 2 framing bytes per event. Per-packet overhead is
    // ~14 B (timestamp + length + headers) — we allocate enough
    // headroom that the entire pool of SysEx events fits in one
    // call to midiOutputCallback.
    Byte *sysexPacketBuf;
    uint32_t sysexPacketBufSize;
    // One-event framing scratch: `0xF0` + inner bytes + `0xF7`.
    // `MIDIPacketListAdd` copies the byte buffer synchronously, so
    // we only need enough space for the single largest event we'll
    // ever pass in. Sized to truce_core::SYSEX_POOL_PREALLOC + 2.
    Byte *sysexFrameScratch;
    uint32_t sysexFrameScratchSize;

    // Host callbacks (set via kAudioUnitProperty_HostCallbacks). Used to
    // query tempo / play state / bar position from the host each render.
    HostCallbackInfo hostCallbacks;
    Boolean hasHostCallbacks;

    // Property listeners
    struct {
        AudioUnitPropertyID prop;
        AudioUnitPropertyListenerProc proc;
        void *userData;
    } listeners[32];
    uint32_t listenerCount;
} TruceAUv2;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

static void build_asbd(AudioStreamBasicDescription *asbd, double sampleRate, uint32_t channels) {
    memset(asbd, 0, sizeof(*asbd));
    asbd->mSampleRate = sampleRate;
    asbd->mFormatID = kAudioFormatLinearPCM;
    asbd->mFormatFlags = kAudioFormatFlagsNativeFloatPacked | kAudioFormatFlagIsNonInterleaved;
    asbd->mBitsPerChannel = 32;
    asbd->mChannelsPerFrame = channels;
    asbd->mFramesPerPacket = 1;
    asbd->mBytesPerFrame = 4;
    asbd->mBytesPerPacket = 4;
}

static void notify_listeners(TruceAUv2 *inst, AudioUnitPropertyID prop,
                             AudioUnitScope scope, AudioUnitElement elem) {
    for (uint32_t i = 0; i < inst->listenerCount; i++) {
        if (inst->listeners[i].prop == prop) {
            inst->listeners[i].proc(inst->listeners[i].userData,
                                     inst->componentInstance, prop, scope, elem);
        }
    }
}

// Global ctx→TruceAUv2* mapping for host notification from GUI callbacks
#define kMaxAUInstances 64
static void *g_au_ctx_keys[kMaxAUInstances] = {0};
static TruceAUv2 *g_au_ctx_vals[kMaxAUInstances] = {0};

static void au_ctx_map_register(void *ctx, TruceAUv2 *inst) {
    for (int i = 0; i < kMaxAUInstances; i++) {
        if (!g_au_ctx_keys[i]) {
            g_au_ctx_keys[i] = ctx;
            g_au_ctx_vals[i] = inst;
            return;
        }
    }
}

static void au_ctx_map_unregister(void *ctx) {
    for (int i = 0; i < kMaxAUInstances; i++) {
        if (g_au_ctx_keys[i] == ctx) {
            g_au_ctx_keys[i] = NULL;
            g_au_ctx_vals[i] = NULL;
            return;
        }
    }
}

static TruceAUv2 *au_ctx_map_lookup(void *ctx) {
    for (int i = 0; i < kMaxAUInstances; i++) {
        if (g_au_ctx_keys[i] == ctx) return g_au_ctx_vals[i];
    }
    return NULL;
}

/* Build the AudioUnitEvent the gesture / value-change helpers all share. */
static void fill_param_event(AudioUnitEvent *event, AudioUnit unit,
                             uint32_t param_id, AudioUnitEventType type) {
    memset(event, 0, sizeof(*event));
    event->mEventType = type;
    event->mArgument.mParameter.mAudioUnit = unit;
    event->mArgument.mParameter.mParameterID = param_id;
    event->mArgument.mParameter.mScope = kAudioUnitScope_Global;
    event->mArgument.mParameter.mElement = 0;
}

/* Called from Rust GUI callback to notify the AU host of a parameter change.
 *
 * Sets the value via `AudioUnitSetParameter` (which updates the host's
 * cached value) and broadcasts a `kAudioUnitEvent_ParameterValueChange`
 * via `AUEventListenerNotify` so any registered AUEventListener sees
 * the change and records automation. `AudioUnitSetParameter` alone does
 * not synthesise the listener notification — hosts that thin / record
 * automation rely on the explicit broadcast. */
void truce_au_v2_host_set_param(void *ctx, uint32_t param_id, float value) {
    TruceAUv2 *inst = au_ctx_map_lookup(ctx);
    if (!inst || !inst->componentInstance) return;
    AudioUnitSetParameter(inst->componentInstance, param_id,
                          kAudioUnitScope_Global, 0, value, 0);
    AudioUnitEvent event;
    fill_param_event(&event, inst->componentInstance, param_id,
                     kAudioUnitEvent_ParameterValueChange);
    AUEventListenerNotify(NULL, NULL, &event);
}

/* Called from Rust GUI callback when the user starts dragging a control.
 *
 * Posts `kAudioUnitEvent_BeginParameterChangeGesture` so hosts (Logic,
 * Live, Reaper) group the subsequent value changes into a single undo
 * step and start gesture-aware automation recording. Without this, every
 * sample of a knob drag becomes a separate undo entry. */
void truce_au_v2_host_begin_param_gesture(void *ctx, uint32_t param_id) {
    TruceAUv2 *inst = au_ctx_map_lookup(ctx);
    if (!inst || !inst->componentInstance) return;
    AudioUnitEvent event;
    fill_param_event(&event, inst->componentInstance, param_id,
                     kAudioUnitEvent_BeginParameterChangeGesture);
    AUEventListenerNotify(NULL, NULL, &event);
}

/* Called from Rust GUI callback when the user releases a control.
 *
 * Posts `kAudioUnitEvent_EndParameterChangeGesture` to close the
 * gesture started by `..._begin_param_gesture`. Hosts use the End event
 * to commit the undo group and stop the automation-recording window. */
void truce_au_v2_host_end_param_gesture(void *ctx, uint32_t param_id) {
    TruceAUv2 *inst = au_ctx_map_lookup(ctx);
    if (!inst || !inst->componentInstance) return;
    AudioUnitEvent event;
    fill_param_event(&event, inst->componentInstance, param_id,
                     kAudioUnitEvent_EndParameterChangeGesture);
    AUEventListenerNotify(NULL, NULL, &event);
}

static int is_instrument(void) {
    if (!g_descriptor) return 0;
    return (g_descriptor->component_type[0] == 'a' &&
            g_descriptor->component_type[1] == 'u' &&
            g_descriptor->component_type[2] == 'm' &&
            g_descriptor->component_type[3] == 'u');
}

/* Append one packet to the in-progress `MIDIPacketList`, flushing
 * to the host and retrying on overflow.
 *
 * Returns the next free `MIDIPacket *` for further appends, or NULL
 * when even an empty list can't hold this packet (the event is
 * dropped — truncating MIDI / `SysEx` is corrupt). On flush the
 * callsite's `*pkt` is replaced with a fresh init pointer.
 *
 * Centralised here so the channel-voice and `SysEx` drains share
 * one overflow policy. The audio thread does the work, so all
 * inputs are stack / pool memory the helper never owns. */
static MIDIPacket *append_or_flush_retry(MIDIPacketList *pktList,
                                         MIDIPacket *pkt,
                                         TruceAUv2 *inst,
                                         const AudioTimeStamp *inTimeStamp,
                                         MIDITimeStamp ts,
                                         ByteCount len,
                                         const Byte *data) {
    if (!pkt) return NULL;
    MIDIPacket *next = MIDIPacketListAdd(
        pktList, inst->sysexPacketBufSize, pkt, ts, len, data);
    if (!next) {
        inst->midiOutputCallback(inst->midiOutputUserData,
                                 inTimeStamp, 0 /* outputIndex */,
                                 pktList);
        pkt = MIDIPacketListInit(pktList);
        next = MIDIPacketListAdd(
            pktList, inst->sysexPacketBufSize, pkt, ts, len, data);
    }
    return next;
}

// ---------------------------------------------------------------------------
// Open / Close
// ---------------------------------------------------------------------------

static OSStatus au_v2_open(void *self_, AudioComponentInstance instance) {
    TruceAUv2 *inst = (TruceAUv2 *)self_;
    inst->componentInstance = instance;

    if (!g_callbacks) return kAudioUnitErr_FailedInitialization;
    inst->rustCtx = g_callbacks->create();
    if (!inst->rustCtx) return kAudioUnitErr_FailedInitialization;
    au_ctx_map_register(inst->rustCtx, inst);

    inst->sampleRate = 44100.0;
    inst->maxFramesPerSlice = 1024;

    if (g_descriptor->num_outputs > 0)
        build_asbd(&inst->outputFormat, inst->sampleRate, g_descriptor->num_outputs);
    if (g_descriptor->num_inputs > 0)
        build_asbd(&inst->inputFormat, inst->sampleRate, g_descriptor->num_inputs);

    // Default preset. presetNumber = -1 is Apple's "no factory preset
    // selected, this is a user / default state" sentinel. The name is
    // CFRetained here and released in au_v2_close.
    inst->currentPreset.presetNumber = -1;
    inst->currentPreset.presetName = CFSTR("Untitled");
    CFRetain(inst->currentPreset.presetName);

    return noErr;
}

static OSStatus au_v2_close(void *self_) {
    TruceAUv2 *inst = (TruceAUv2 *)self_;
    if (inst->rustCtx && g_callbacks) {
        au_ctx_map_unregister(inst->rustCtx);
        g_callbacks->destroy(inst->rustCtx);
        inst->rustCtx = NULL;
    }
    for (int c = 0; c < 32; c++) {
        free(inst->outputBuffers[c]);
        inst->outputBuffers[c] = NULL;
    }
    free(inst->sysexPacketBuf);
    inst->sysexPacketBuf = NULL;
    free(inst->sysexFrameScratch);
    inst->sysexFrameScratch = NULL;
    if (inst->currentPreset.presetName) {
        CFRelease(inst->currentPreset.presetName);
        inst->currentPreset.presetName = NULL;
    }
    free(inst);
    return noErr;
}

// ---------------------------------------------------------------------------
// Initialize / Uninitialize / Reset
// ---------------------------------------------------------------------------

static OSStatus au_v2_initialize(void *self_) {
    TruceAUv2 *inst = (TruceAUv2 *)self_;
    if (g_callbacks && inst->rustCtx)
        g_callbacks->reset(inst->rustCtx, inst->sampleRate, inst->maxFramesPerSlice);

    // Allocate internal output buffers
    uint32_t numOut = g_descriptor->num_outputs;
    for (uint32_t c = 0; c < numOut && c < 32; c++) {
        free(inst->outputBuffers[c]);
        inst->outputBuffers[c] = (float *)calloc(inst->maxFramesPerSlice, sizeof(float));
    }
    inst->outputBufferSize = inst->maxFramesPerSlice;

    inst->initialized = true;
    return noErr;
}

static OSStatus au_v2_uninitialize(void *self_) {
    TruceAUv2 *inst = (TruceAUv2 *)self_;
    inst->initialized = false;
    return noErr;
}

static OSStatus au_v2_reset(void *self_, AudioUnitScope scope, AudioUnitElement elem) {
    (void)scope; (void)elem;
    TruceAUv2 *inst = (TruceAUv2 *)self_;
    if (g_callbacks && inst->rustCtx)
        g_callbacks->reset(inst->rustCtx, inst->sampleRate, inst->maxFramesPerSlice);
    inst->midiCount = 0;
    return noErr;
}

// ---------------------------------------------------------------------------
// GetPropertyInfo / GetProperty / SetProperty
// ---------------------------------------------------------------------------

static OSStatus au_v2_get_property_info(void *self_, AudioUnitPropertyID prop,
                                         AudioUnitScope scope, AudioUnitElement elem,
                                         UInt32 *outSize, Boolean *outWritable) {
    (void)self_; (void)elem;
    UInt32 size = 0;
    Boolean writable = false;

    switch (prop) {
        case kAudioUnitProperty_StreamFormat:
            size = sizeof(AudioStreamBasicDescription); writable = true; break;
        case kAudioUnitProperty_ElementCount:
            size = sizeof(UInt32); break;
        case kAudioUnitProperty_SampleRate:
            size = sizeof(Float64); writable = true; break;
        case kAudioUnitProperty_MaximumFramesPerSlice:
            size = sizeof(UInt32); writable = true; break;
        case kAudioUnitProperty_ParameterList:
            if (scope == kAudioUnitScope_Global)
                size = g_num_params * sizeof(AudioUnitParameterID);
            else
                size = 0; // params only on Global scope
            break;
        case kAudioUnitProperty_ParameterInfo:
            size = sizeof(AudioUnitParameterInfo); break;
        case kAudioUnitProperty_SetRenderCallback:
            if (scope == kAudioUnitScope_Input) { size = sizeof(AURenderCallbackStruct); writable = true; }
            else return kAudioUnitErr_InvalidScope;
            break;
        case kAudioUnitProperty_MakeConnection:
            if (scope == kAudioUnitScope_Input) { size = sizeof(AudioUnitConnection); writable = true; }
            else return kAudioUnitErr_InvalidScope;
            break;
        case kAudioUnitProperty_ShouldAllocateBuffer:
            size = sizeof(UInt32); writable = true; break;
        case kAudioUnitProperty_HostCallbacks:
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            size = sizeof(HostCallbackInfo); writable = true; break;
        case kAudioUnitProperty_MIDIOutputCallbackInfo:
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            size = sizeof(CFArrayRef); writable = false; break;
        case kAudioUnitProperty_MIDIOutputCallback:
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            size = sizeof(AUMIDIOutputCallbackStruct); writable = true; break;
        case kAudioUnitProperty_ClassInfo:
            size = sizeof(CFPropertyListRef); writable = true; break;
        case kAudioUnitProperty_PresentPreset:
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            size = sizeof(AUPreset); writable = true; break;
        case kAudioUnitProperty_Latency:
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            size = sizeof(Float64); break;
        case kAudioUnitProperty_TailTime:
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            size = sizeof(Float64); break;
        case kAudioUnitProperty_BypassEffect:
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            size = sizeof(UInt32); writable = true; break;
        case kAudioUnitProperty_LastRenderError:
            size = sizeof(OSStatus); break;
        case kAudioUnitProperty_InPlaceProcessing:
            size = sizeof(UInt32); break;
        case kAudioUnitProperty_SupportedNumChannels:
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            size = sizeof(AUChannelInfo); break;
        case kAudioUnitProperty_CocoaUI: {
            TruceAUv2 *inst = (TruceAUv2 *)self_;
            if (!g_callbacks || !inst->rustCtx) return kAudioUnitErr_InvalidProperty;
            if (!g_callbacks->gui_has_editor(inst->rustCtx)) return kAudioUnitErr_InvalidProperty;
            size = sizeof(AudioUnitCocoaViewInfo); break;
        }
        case 64000: /* kTrucePrivateProperty_RustContext */
        case 64001: /* kTrucePrivateProperty_AuCallbacks */
            size = sizeof(void*); break;
        default:
            return kAudioUnitErr_InvalidProperty;
    }

    if (outSize) *outSize = size;
    if (outWritable) *outWritable = writable;
    return noErr;
}

static OSStatus au_v2_get_property(void *self_, AudioUnitPropertyID prop,
                                    AudioUnitScope scope, AudioUnitElement elem,
                                    void *outData, UInt32 *ioSize) {
    TruceAUv2 *inst = (TruceAUv2 *)self_;

    switch (prop) {
        case kAudioUnitProperty_StreamFormat: {
            if (*ioSize < sizeof(AudioStreamBasicDescription))
                return kAudioUnitErr_InvalidPropertyValue;
            AudioStreamBasicDescription *asbd = (AudioStreamBasicDescription *)outData;
            if (scope == kAudioUnitScope_Output) {
                if (g_descriptor->num_outputs == 0)
                    return kAudioUnitErr_InvalidElement;
                *asbd = inst->outputFormat;
            } else if (scope == kAudioUnitScope_Input) {
                if (g_descriptor->num_inputs == 0)
                    return kAudioUnitErr_InvalidElement;
                *asbd = inst->inputFormat;
            } else {
                return kAudioUnitErr_InvalidScope;
            }
            *ioSize = sizeof(AudioStreamBasicDescription);
            return noErr;
        }

        case kAudioUnitProperty_ElementCount: {
            UInt32 *count = (UInt32 *)outData;
            if (scope == kAudioUnitScope_Input)
                *count = (g_descriptor->num_inputs > 0) ? 1 : 0;
            else if (scope == kAudioUnitScope_Output)
                *count = (g_descriptor->num_outputs > 0) ? 1 : 0;
            else if (scope == kAudioUnitScope_Global)
                *count = 1;
            else return kAudioUnitErr_InvalidScope;
            *ioSize = sizeof(UInt32);
            return noErr;
        }

        case kAudioUnitProperty_SampleRate: {
            *(Float64 *)outData = inst->sampleRate;
            *ioSize = sizeof(Float64);
            return noErr;
        }

        case kAudioUnitProperty_MaximumFramesPerSlice: {
            *(UInt32 *)outData = inst->maxFramesPerSlice;
            *ioSize = sizeof(UInt32);
            return noErr;
        }

        case kAudioUnitProperty_ParameterList: {
            if (scope != kAudioUnitScope_Global) {
                // Return empty list for non-global scopes
                *ioSize = 0;
                return noErr;
            }
            UInt32 needed = g_num_params * sizeof(AudioUnitParameterID);
            if (*ioSize < needed) return kAudioUnitErr_InvalidPropertyValue;
            AudioUnitParameterID *ids = (AudioUnitParameterID *)outData;
            for (uint32_t i = 0; i < g_num_params; i++)
                ids[i] = g_param_descriptors[i].id;
            *ioSize = needed;
            return noErr;
        }

        case kAudioUnitProperty_ParameterInfo: {
            if (*ioSize < sizeof(AudioUnitParameterInfo))
                return kAudioUnitErr_InvalidPropertyValue;
            AudioUnitParameterInfo *info = (AudioUnitParameterInfo *)outData;
            memset(info, 0, sizeof(*info));
            for (uint32_t i = 0; i < g_num_params; i++) {
                if (g_param_descriptors[i].id == elem) {
                    const AuParamDescriptor *pd = &g_param_descriptors[i];
                    strlcpy(info->name, pd->name, sizeof(info->name));
                    info->cfNameString = CFStringCreateWithCString(NULL, pd->name,
                                            kCFStringEncodingUTF8);
                    info->unit = kAudioUnitParameterUnit_Generic;
                    info->minValue = (AudioUnitParameterValue)pd->min;
                    info->maxValue = (AudioUnitParameterValue)pd->max;
                    info->defaultValue = (AudioUnitParameterValue)pd->default_value;
                    info->flags = kAudioUnitParameterFlag_IsReadable |
                                  kAudioUnitParameterFlag_IsWritable |
                                  kAudioUnitParameterFlag_HasCFNameString |
                                  kAudioUnitParameterFlag_CFNameRelease;
                    *ioSize = sizeof(AudioUnitParameterInfo);
                    return noErr;
                }
            }
            return kAudioUnitErr_InvalidParameter;
        }

        case kAudioUnitProperty_ShouldAllocateBuffer: {
            *(UInt32 *)outData = 1; // AU allocates its own buffers
            *ioSize = sizeof(UInt32);
            return noErr;
        }

        case kAudioUnitProperty_Latency: {
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            *(Float64 *)outData = 0.0;
            *ioSize = sizeof(Float64);
            return noErr;
        }

        case kAudioUnitProperty_TailTime: {
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            *(Float64 *)outData = 0.0;
            *ioSize = sizeof(Float64);
            return noErr;
        }

        case kAudioUnitProperty_BypassEffect: {
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            UInt32 bypassed = 0;
            if (g_descriptor->bypass_param_id != UINT32_MAX
                && g_callbacks && inst->rustCtx) {
                /* IS_BYPASS-flagged param: 0 = inactive, 1 = bypassed.
                 * Plain value is 0/1 for BoolParam (the common case). */
                double v = g_callbacks->param_get_value(
                    inst->rustCtx, g_descriptor->bypass_param_id);
                bypassed = (v >= 0.5) ? 1 : 0;
            }
            *(UInt32 *)outData = bypassed;
            *ioSize = sizeof(UInt32);
            return noErr;
        }

        case kAudioUnitProperty_LastRenderError: {
            *(OSStatus *)outData = noErr;
            *ioSize = sizeof(OSStatus);
            return noErr;
        }

        case kAudioUnitProperty_MIDIOutputCallbackInfo: {
            /* Hosts that read this expect a CFArray of CFString port
             * names — one entry per logical MIDI output port. truce
             * exposes a single port; "Truce MIDI Out" is the visible
             * label in the host's MIDI routing UI. The CFArray
             * ownership transfers to the caller. */
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            if (*ioSize < sizeof(CFArrayRef))
                return kAudioUnitErr_InvalidPropertyValue;
            CFStringRef portName = CFSTR("Truce MIDI Out");
            CFArrayRef arr = CFArrayCreate(kCFAllocatorDefault,
                                           (const void **)&portName, 1,
                                           &kCFTypeArrayCallBacks);
            *(CFArrayRef *)outData = arr;
            *ioSize = sizeof(CFArrayRef);
            return noErr;
        }

        case kAudioUnitProperty_MIDIOutputCallback: {
            /* Hosts typically only set this property; AU validators
             * sometimes read it back. Return what we have stored. */
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            if (*ioSize < sizeof(AUMIDIOutputCallbackStruct))
                return kAudioUnitErr_InvalidPropertyValue;
            AUMIDIOutputCallbackStruct *cb = (AUMIDIOutputCallbackStruct *)outData;
            cb->midiOutputCallback = inst->midiOutputCallback;
            cb->userData = inst->midiOutputUserData;
            *ioSize = sizeof(AUMIDIOutputCallbackStruct);
            return noErr;
        }

        case kAudioUnitProperty_InPlaceProcessing: {
            // Only effects (with inputs) support in-place processing
            *(UInt32 *)outData = (g_descriptor->num_inputs > 0) ? 1 : 0;
            *ioSize = sizeof(UInt32);
            return noErr;
        }

        case kAudioUnitProperty_SupportedNumChannels: {
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            if (*ioSize < sizeof(AUChannelInfo))
                return kAudioUnitErr_InvalidPropertyValue;
            AUChannelInfo *info = (AUChannelInfo *)outData;
            info->inChannels = (SInt16)g_descriptor->num_inputs;
            info->outChannels = (SInt16)g_descriptor->num_outputs;
            *ioSize = sizeof(AUChannelInfo);
            return noErr;
        }

        case kAudioUnitProperty_CocoaUI: {
            if (*ioSize < sizeof(AudioUnitCocoaViewInfo))
                return kAudioUnitErr_InvalidPropertyValue;

            AudioUnitCocoaViewInfo *viewInfo = (AudioUnitCocoaViewInfo *)outData;

            // Get our .component bundle URL via the AudioUnit's component
            AudioComponent comp = AudioComponentInstanceGetComponent(inst->componentInstance);
            CFStringRef bundleID = NULL;
            AudioComponentCopyName(comp, &bundleID);

            // Use dladdr to find our bundle path
            Dl_info dlInfo;
            if (dladdr((void*)au_v2_get_property, &dlInfo)) {
                // Walk up from the binary path to the .component bundle
                char path[2048];
                strncpy(path, dlInfo.dli_fname, sizeof(path));
                // Go up 3 levels: MacOS → Contents → *.component
                for (int i = 0; i < 3; i++) {
                    char *last = strrchr(path, '/');
                    if (last) *last = 0;
                }
                CFStringRef pathStr = CFStringCreateWithCString(NULL, path, kCFStringEncodingUTF8);
                viewInfo->mCocoaAUViewBundleLocation = CFURLCreateWithFileSystemPath(
                    NULL, pathStr, kCFURLPOSIXPathStyle, true);
                CFRelease(pathStr);
            }
            if (bundleID) CFRelease(bundleID);

            // The Cocoa view factory class is defined in
            // `au_v2_view.m` and registered with the ObjC runtime
            // automatically when the dylib loads (via the compiler's
            // `__objc_classlist`). `truce_au_view_factory_class_name`
            // returns this dylib's unique class name — see au_v2_view.m
            // for why each plugin needs its own.
            extern const char *truce_au_view_factory_class_name(void);
            viewInfo->mCocoaAUViewClass[0] = CFStringCreateWithCString(
                NULL, truce_au_view_factory_class_name(), kCFStringEncodingUTF8);

            *ioSize = sizeof(AudioUnitCocoaViewInfo);
            return noErr;
        }

        case 64000: { /* kTrucePrivateProperty_RustContext */
            *(void **)outData = inst->rustCtx;
            *ioSize = sizeof(void*);
            return noErr;
        }

        case 64001: { /* kTrucePrivateProperty_AuCallbacks */
            /* The Cocoa view methods (see au_v2_view.m) read `rustCtx`
             * and the callbacks table through the AU dispatch table
             * rather than touching dylib globals directly. That keeps
             * the view shim source identical across plugins, even
             * though each plugin compiles its own uniquely-named
             * class — every property fetch lands in the dylib that
             * owns the AU instance, which is always the correct one. */
            *(const AuCallbacks **)outData = g_callbacks;
            *ioSize = sizeof(void*);
            return noErr;
        }

        case kAudioUnitProperty_PresentPreset: {
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            if (*ioSize < sizeof(AUPreset)) return kAudioUnitErr_InvalidPropertyValue;
            // Hand the caller a struct copy. The host receives ownership
            // of the name reference and is expected to CFRelease it,
            // so we balance with an extra CFRetain here.
            *(AUPreset *)outData = inst->currentPreset;
            if (inst->currentPreset.presetName) {
                CFRetain(inst->currentPreset.presetName);
            }
            *ioSize = sizeof(AUPreset);
            return noErr;
        }

        case kAudioUnitProperty_ClassInfo: {
            // State save: build a CFDictionary using Apple's standard
            // preset keys. The "name" slot carries the current preset
            // name (set by the host via kAudioUnitProperty_PresentPreset
            // or by a prior ClassInfo round-trip), not the component
            // name. auval's "preset name is not retained" check reads
            // this slot.
            if (!g_callbacks || !inst->rustCtx) return kAudioUnitErr_Uninitialized;

            CFMutableDictionaryRef dict = CFDictionaryCreateMutable(NULL, 0,
                &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);

            // Standard AU keys
            SInt32 compType = (SInt32)FOURCC(g_descriptor->component_type);
            SInt32 compSubType = (SInt32)FOURCC(g_descriptor->component_subtype);
            SInt32 compMfr = (SInt32)FOURCC(g_descriptor->component_manufacturer);
            SInt32 compVer = (SInt32)g_descriptor->version;

            CFNumberRef nType = CFNumberCreate(NULL, kCFNumberSInt32Type, &compType);
            CFNumberRef nSub = CFNumberCreate(NULL, kCFNumberSInt32Type, &compSubType);
            CFNumberRef nMfr = CFNumberCreate(NULL, kCFNumberSInt32Type, &compMfr);
            CFNumberRef nVer = CFNumberCreate(NULL, kCFNumberSInt32Type, &compVer);
            CFStringRef sName = inst->currentPreset.presetName
                ? (CFStringRef)CFRetain(inst->currentPreset.presetName)
                : CFSTR("Untitled");

            CFDictionarySetValue(dict, CFSTR("type"), nType);
            CFDictionarySetValue(dict, CFSTR("subtype"), nSub);
            CFDictionarySetValue(dict, CFSTR("manufacturer"), nMfr);
            CFDictionarySetValue(dict, CFSTR("version"), nVer);
            CFDictionarySetValue(dict, CFSTR("name"), sName);

            CFRelease(nType); CFRelease(nSub); CFRelease(nMfr); CFRelease(nVer);
            if (inst->currentPreset.presetName) CFRelease(sName);

            // Plugin state blob
            uint8_t *data = NULL; uint32_t len = 0;
            g_callbacks->state_save(inst->rustCtx, &data, &len);
            if (data && len > 0) {
                CFDataRef cfData = CFDataCreate(NULL, data, len);
                CFDictionarySetValue(dict, CFSTR("truce_state"), cfData);
                CFRelease(cfData);
                g_callbacks->state_free(data, len);
            }

            *(CFPropertyListRef *)outData = dict;
            *ioSize = sizeof(CFPropertyListRef);
            return noErr;
        }

        default:
            return kAudioUnitErr_InvalidProperty;
    }
}

static OSStatus au_v2_set_property(void *self_, AudioUnitPropertyID prop,
                                    AudioUnitScope scope, AudioUnitElement elem,
                                    const void *inData, UInt32 inSize) {
    TruceAUv2 *inst = (TruceAUv2 *)self_;
    (void)elem;

    switch (prop) {
        case kAudioUnitProperty_StreamFormat: {
            if (inSize < sizeof(AudioStreamBasicDescription))
                return kAudioUnitErr_InvalidPropertyValue;
            const AudioStreamBasicDescription *asbd = (const AudioStreamBasicDescription *)inData;
            // Validate: must be non-interleaved float32 PCM
            if (asbd->mFormatID != kAudioFormatLinearPCM) return kAudioUnitErr_FormatNotSupported;
            if (!(asbd->mFormatFlags & kAudioFormatFlagIsFloat)) return kAudioUnitErr_FormatNotSupported;
            if (!(asbd->mFormatFlags & kAudioFormatFlagIsNonInterleaved)) return kAudioUnitErr_FormatNotSupported;

            // Validate channel count matches our bus configuration
            if (scope == kAudioUnitScope_Output) {
                if (g_descriptor->num_outputs == 0)
                    return kAudioUnitErr_InvalidElement;
                if (asbd->mChannelsPerFrame != g_descriptor->num_outputs)
                    return kAudioUnitErr_FormatNotSupported;
                inst->outputFormat = *asbd;
                inst->sampleRate = asbd->mSampleRate;
            } else if (scope == kAudioUnitScope_Input) {
                if (g_descriptor->num_inputs == 0)
                    return kAudioUnitErr_InvalidElement;
                if (asbd->mChannelsPerFrame != g_descriptor->num_inputs)
                    return kAudioUnitErr_FormatNotSupported;
                inst->inputFormat = *asbd;
            } else {
                return kAudioUnitErr_InvalidScope;
            }
            return noErr;
        }

        case kAudioUnitProperty_SampleRate: {
            inst->sampleRate = *(const Float64 *)inData;
            inst->outputFormat.mSampleRate = inst->sampleRate;
            inst->inputFormat.mSampleRate = inst->sampleRate;
            return noErr;
        }

        case kAudioUnitProperty_MaximumFramesPerSlice: {
            inst->maxFramesPerSlice = *(const UInt32 *)inData;
            notify_listeners(inst, kAudioUnitProperty_MaximumFramesPerSlice,
                           kAudioUnitScope_Global, 0);
            return noErr;
        }

        case kAudioUnitProperty_SetRenderCallback: {
            if (scope != kAudioUnitScope_Input) return kAudioUnitErr_InvalidScope;
            const AURenderCallbackStruct *cb = (const AURenderCallbackStruct *)inData;
            inst->inputCallback = cb->inputProc;
            inst->inputCallbackRefCon = cb->inputProcRefCon;
            inst->sourceUnit = NULL;
            return noErr;
        }

        case kAudioUnitProperty_MakeConnection: {
            if (scope != kAudioUnitScope_Input) return kAudioUnitErr_InvalidScope;
            const AudioUnitConnection *conn = (const AudioUnitConnection *)inData;
            inst->sourceUnit = conn->sourceAudioUnit;
            inst->sourceOutputBus = conn->sourceOutputNumber;
            inst->inputCallback = NULL;
            inst->inputCallbackRefCon = NULL;
            return noErr;
        }

        case kAudioUnitProperty_ShouldAllocateBuffer:
            return noErr; // accept but ignore

        case kAudioUnitProperty_HostCallbacks: {
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            // Host may pass a shorter struct than sizeof(HostCallbackInfo);
            // copy only what was supplied. The unused function pointers
            // stay NULL, which is what the SDK sentinel value means.
            memset(&inst->hostCallbacks, 0, sizeof(inst->hostCallbacks));
            UInt32 copy = inSize < sizeof(inst->hostCallbacks) ? inSize : sizeof(inst->hostCallbacks);
            memcpy(&inst->hostCallbacks, inData, copy);
            inst->hasHostCallbacks = (inst->hostCallbacks.beatAndTempoProc ||
                                      inst->hostCallbacks.musicalTimeLocationProc ||
                                      inst->hostCallbacks.transportStateProc ||
                                      inst->hostCallbacks.transportStateProc2);
            return noErr;
        }

        case kAudioUnitProperty_MIDIOutputCallback: {
            /* Host registers its MIDI output callback. We stash it
             * and call it after each render block with whatever
             * events the plugin pushed into `output_events`. */
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            if (!inData || inSize < sizeof(AUMIDIOutputCallbackStruct))
                return kAudioUnitErr_InvalidPropertyValue;
            const AUMIDIOutputCallbackStruct *cb =
                (const AUMIDIOutputCallbackStruct *)inData;
            inst->midiOutputCallback = cb->midiOutputCallback;
            inst->midiOutputUserData = cb->userData;
            return noErr;
        }

        case kAudioUnitProperty_BypassEffect: {
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            if (!inData || inSize < sizeof(UInt32))
                return kAudioUnitErr_InvalidPropertyValue;
            UInt32 bypassed = *(const UInt32 *)inData;
            if (g_descriptor->bypass_param_id != UINT32_MAX
                && g_callbacks && inst->rustCtx) {
                g_callbacks->param_set_value(
                    inst->rustCtx,
                    g_descriptor->bypass_param_id,
                    bypassed ? 1.0 : 0.0);
            }
            return noErr;
        }
        case kAudioUnitProperty_PresentPreset: {
            if (scope != kAudioUnitScope_Global) return kAudioUnitErr_InvalidScope;
            if (!inData || inSize < sizeof(AUPreset))
                return kAudioUnitErr_InvalidPropertyValue;
            const AUPreset *src = (const AUPreset *)inData;
            // Release the old name and retain the incoming one. The
            // host owns its copy independently of ours.
            if (inst->currentPreset.presetName) {
                CFRelease(inst->currentPreset.presetName);
                inst->currentPreset.presetName = NULL;
            }
            inst->currentPreset.presetNumber = src->presetNumber;
            if (src->presetName) {
                inst->currentPreset.presetName = (CFStringRef)CFRetain(src->presetName);
            }
            notify_listeners(inst, kAudioUnitProperty_PresentPreset,
                             kAudioUnitScope_Global, 0);
            return noErr;
        }

        case kAudioUnitProperty_ClassInfo: {
            // State load
            if (!g_callbacks || !inst->rustCtx) return kAudioUnitErr_Uninitialized;
            if (!inData || inSize < sizeof(CFPropertyListRef))
                return kAudioUnitErr_InvalidPropertyValue;

            CFPropertyListRef plist = *(const CFPropertyListRef *)inData;
            if (!plist) return kAudioUnitErr_InvalidPropertyValue;

            // Verify it's actually a dictionary
            if (CFGetTypeID(plist) != CFDictionaryGetTypeID())
                return kAudioUnitErr_InvalidPropertyValue;

            CFDictionaryRef dict = (CFDictionaryRef)plist;

            // Round-trip the preset name through the standard "name"
            // slot so auval's "preset name is not retained" check
            // passes. We accept whatever the host wrote into ClassInfo
            // even if it never called PresentPreset.
            CFStringRef savedName = CFDictionaryGetValue(dict, CFSTR("name"));
            if (savedName && CFGetTypeID(savedName) == CFStringGetTypeID()) {
                if (inst->currentPreset.presetName) {
                    CFRelease(inst->currentPreset.presetName);
                }
                inst->currentPreset.presetName = (CFStringRef)CFRetain(savedName);
                // A user-loaded blob isn't tied to a factory preset
                // index, so reset the number to -1 per Apple convention.
                inst->currentPreset.presetNumber = -1;
                notify_listeners(inst, kAudioUnitProperty_PresentPreset,
                                 kAudioUnitScope_Global, 0);
            }

            CFDataRef cfData = CFDictionaryGetValue(dict, CFSTR("truce_state"));
            if (cfData && CFGetTypeID(cfData) == CFDataGetTypeID()) {
                const uint8_t *bytes = CFDataGetBytePtr(cfData);
                uint32_t len = (uint32_t)CFDataGetLength(cfData);
                g_callbacks->state_load(inst->rustCtx, bytes, len);
            }
            return noErr;
        }

        default:
            return kAudioUnitErr_InvalidProperty;
    }
}

// ---------------------------------------------------------------------------
// Get/Set Parameter
// ---------------------------------------------------------------------------

static OSStatus au_v2_get_parameter(void *self_, AudioUnitParameterID id,
                                     AudioUnitScope scope, AudioUnitElement elem,
                                     AudioUnitParameterValue *outValue) {
    (void)scope; (void)elem;
    TruceAUv2 *inst = (TruceAUv2 *)self_;
    if (!g_callbacks || !inst->rustCtx) return kAudioUnitErr_Uninitialized;
    *outValue = (AudioUnitParameterValue)g_callbacks->param_get_value(inst->rustCtx, id);
    return noErr;
}

static OSStatus au_v2_set_parameter(void *self_, AudioUnitParameterID id,
                                     AudioUnitScope scope, AudioUnitElement elem,
                                     AudioUnitParameterValue value, UInt32 bufferOffset) {
    (void)scope; (void)elem; (void)bufferOffset;
    TruceAUv2 *inst = (TruceAUv2 *)self_;
    if (!g_callbacks || !inst->rustCtx) return kAudioUnitErr_Uninitialized;
    g_callbacks->param_set_value(inst->rustCtx, id, (double)value);
    return noErr;
}

static OSStatus au_v2_schedule_parameters(void *self_,
                                           const AudioUnitParameterEvent *events,
                                           UInt32 numEvents) {
    TruceAUv2 *inst = (TruceAUv2 *)self_;
    if (!g_callbacks || !inst->rustCtx) return kAudioUnitErr_Uninitialized;
    for (UInt32 i = 0; i < numEvents; i++) {
        if (events[i].eventType == kParameterEvent_Immediate) {
            g_callbacks->param_set_value(inst->rustCtx,
                events[i].parameter, (double)events[i].eventValues.immediate.value);
        }
    }
    return noErr;
}

// ---------------------------------------------------------------------------
// Render
// ---------------------------------------------------------------------------

/* Fill `out` from HostCallbackInfo. Each proc is optional — missing
 * callbacks leave their corresponding fields at zero. `valid` is set
 * to 1 as long as at least one proc returned successfully. */
static void fill_transport_snapshot(TruceAUv2 *inst,
                                     const AudioTimeStamp *ts,
                                     AuTransportSnapshot *out) {
    memset(out, 0, sizeof(*out));
    if (!inst->hasHostCallbacks) return;
    void *ud = inst->hostCallbacks.hostUserData;
    int ok = 0;

    if (inst->hostCallbacks.beatAndTempoProc) {
        Float64 beat = 0.0, tempo = 0.0;
        if (inst->hostCallbacks.beatAndTempoProc(ud, &beat, &tempo) == noErr) {
            out->position_beats = beat;
            out->tempo = tempo;
            ok = 1;
        }
    }
    if (inst->hostCallbacks.musicalTimeLocationProc) {
        UInt32 delta = 0;
        Float32 tsig_num = 0.0f;
        UInt32 tsig_den = 0;
        Float64 downbeat = 0.0;
        if (inst->hostCallbacks.musicalTimeLocationProc(ud, &delta, &tsig_num,
                                                         &tsig_den, &downbeat) == noErr) {
            out->time_sig_num = (int32_t)tsig_num;
            out->time_sig_den = (int32_t)tsig_den;
            out->bar_start_beats = downbeat;
            ok = 1;
        }
    }
    if (inst->hostCallbacks.transportStateProc2) {
        Boolean playing = false, recording = false, cycling = false, changed = false;
        Float64 samplePos = 0.0, cycleStart = 0.0, cycleEnd = 0.0;
        if (inst->hostCallbacks.transportStateProc2(ud, &playing, &recording,
                                                     &changed, &samplePos,
                                                     &cycling, &cycleStart, &cycleEnd) == noErr) {
            out->playing = playing ? 1 : 0;
            out->recording = recording ? 1 : 0;
            out->loop_active = cycling ? 1 : 0;
            out->position_samples = samplePos;
            out->loop_start_beats = cycleStart;
            out->loop_end_beats = cycleEnd;
            ok = 1;
        }
    } else if (inst->hostCallbacks.transportStateProc) {
        Boolean playing = false, changed = false, cycling = false;
        Float64 samplePos = 0.0, cycleStart = 0.0, cycleEnd = 0.0;
        if (inst->hostCallbacks.transportStateProc(ud, &playing, &changed,
                                                    &samplePos, &cycling,
                                                    &cycleStart, &cycleEnd) == noErr) {
            out->playing = playing ? 1 : 0;
            out->loop_active = cycling ? 1 : 0;
            out->position_samples = samplePos;
            out->loop_start_beats = cycleStart;
            out->loop_end_beats = cycleEnd;
            ok = 1;
        }
    }
    if (ok == 0 && ts && (ts->mFlags & kAudioTimeStampSampleTimeValid)) {
        // Fall back to the render timestamp if the host has no transport
        // procs — at least gives the plugin a sample position.
        out->position_samples = ts->mSampleTime;
        ok = 1;
    }
    out->valid = ok;
}

static OSStatus au_v2_render(void *self_,
                              AudioUnitRenderActionFlags *ioFlags,
                              const AudioTimeStamp *inTimeStamp,
                              UInt32 inBusNumber,
                              UInt32 inFrameCount,
                              AudioBufferList *ioData) {
    (void)inBusNumber;
    TruceAUv2 *inst = (TruceAUv2 *)self_;
    if (!inst->initialized || !g_callbacks || !inst->rustCtx)
        return kAudioUnitErr_Uninitialized;

    // Clear the output-is-silence flag — we produce audio
    if (ioFlags) *ioFlags &= ~kAudioUnitRenderAction_OutputIsSilence;

    if (inFrameCount > inst->maxFramesPerSlice)
        return kAudioUnitErr_TooManyFramesToProcess;


    // Pull input for effects
    uint32_t numIn = g_descriptor->num_inputs;
    uint32_t numOut = g_descriptor->num_outputs;

    if (numIn > 0) {
        // Build a temporary ABL pointing to our buffers for the input pull
        UInt32 pullBufCount = numIn < ioData->mNumberBuffers ? numIn : ioData->mNumberBuffers;
        // Use stack-allocated ABL
        char ablStorage[sizeof(AudioBufferList) + sizeof(AudioBuffer) * 31];
        AudioBufferList *pullABL = (AudioBufferList *)ablStorage;
        pullABL->mNumberBuffers = pullBufCount;
        for (UInt32 c = 0; c < pullBufCount; c++) {
            pullABL->mBuffers[c].mNumberChannels = 1;
            pullABL->mBuffers[c].mDataByteSize = inFrameCount * sizeof(float);
            pullABL->mBuffers[c].mData = inst->outputBuffers[c]; // pull into our buffers
        }

        if (inst->inputCallback) {
            AudioUnitRenderActionFlags pullFlags = 0;
            OSStatus err = inst->inputCallback(inst->inputCallbackRefCon,
                &pullFlags, inTimeStamp, 0, inFrameCount, pullABL);
            if (err != noErr) return err;
            // Copy back any pointer changes the callback may have made
            for (UInt32 c = 0; c < pullBufCount; c++) {
                if (pullABL->mBuffers[c].mData != inst->outputBuffers[c]) {
                    memcpy(inst->outputBuffers[c], pullABL->mBuffers[c].mData,
                           inFrameCount * sizeof(float));
                }
            }
        } else if (inst->sourceUnit) {
            AudioUnitRenderActionFlags pullFlags = 0;
            OSStatus err = AudioUnitRender(inst->sourceUnit, &pullFlags,
                inTimeStamp, inst->sourceOutputBus, inFrameCount, pullABL);
            if (err != noErr) return err;
            for (UInt32 c = 0; c < pullBufCount; c++) {
                if (pullABL->mBuffers[c].mData != inst->outputBuffers[c]) {
                    memcpy(inst->outputBuffers[c], pullABL->mBuffers[c].mData,
                           inFrameCount * sizeof(float));
                }
            }
        } else {
            for (uint32_t c = 0; c < pullBufCount; c++)
                memset(inst->outputBuffers[c], 0, inFrameCount * sizeof(float));
        }
    }

    // Save host's original buffer pointers — we MUST write back to these
    void *hostBufs[32] = {0};
    for (uint32_t c = 0; c < ioData->mNumberBuffers && c < 32; c++)
        hostBufs[c] = ioData->mBuffers[c].mData;

    // Build channel pointers — process into our internal buffers
    const float *inPtrs[32];
    float *outPtrs[32];

    for (uint32_t c = 0; c < numIn && c < 32; c++)
        inPtrs[c] = (const float *)inst->outputBuffers[c];
    for (uint32_t c = 0; c < numOut && c < 32; c++)
        outPtrs[c] = inst->outputBuffers[c];


    AuTransportSnapshot transport;
    fill_transport_snapshot(inst, inTimeStamp, &transport);

    /* AU v2 hosts deliver MIDI exclusively through the legacy
     * `MusicDeviceMIDIEvent` path (3-byte MIDI 1.0); they don't have a
     * MIDIEventList equivalent. Forward NULL / 0 for the MIDI 2.0
     * UMP array so the Rust event-decoder skips it. */
    g_callbacks->process(inst->rustCtx, inPtrs, outPtrs,
                         numIn, numOut, inFrameCount,
                         inst->midiBuffer, inst->midiCount,
                         NULL, 0,
                         &transport);
    inst->midiCount = 0;

    /* Drain plugin → host MIDI. Channel-voice events go through
     * `output_event_at` (filtered to fit in 3-byte MIDI 1.0
     * packets); SysEx events go through `output_sysex_at` with
     * inner bytes the shim wraps in `0xF0` / `0xF7` framing before
     * the MIDIPacketListAdd call. Both end up in the same
     * MIDIPacketList so the host callback fires once per render
     * block. Events with `sample_offset >= inFrameCount`
     * (out-of-block) are clamped rather than dropped; AU hosts
     * schedule these for the boundary sample. */
    if (inst->midiOutputCallback) {
        uint32_t cv_count = g_callbacks->output_event_count(inst->rustCtx);
        uint32_t sx_count = g_callbacks->output_sysex_count(inst->rustCtx);
        if (cv_count > 256) cv_count = 256;
        if (sx_count > 256) sx_count = 256;
        if (cv_count > 0 || sx_count > 0) {
            MIDIPacketList *pktList =
                (MIDIPacketList *)inst->sysexPacketBuf;
            MIDIPacket *pkt = MIDIPacketListInit(pktList);

            /* Channel-voice drain. `append_or_flush_retry` handles
             * the overflow path: on `MIDIPacketListAdd` failure it
             * sends the partial list to the host, reinits, and
             * retries the current event. Both drains share the
             * helper so the overflow policy stays in one place. */
            for (uint32_t i = 0; pkt && i < cv_count; i++) {
                AuMidiEvent ev = {0};
                g_callbacks->output_event_at(inst->rustCtx, i, &ev);
                Byte data[3] = { ev.status, ev.data1, ev.data2 };
                /* CC / channel pressure / program change are 2-byte
                 * messages; emit only the bytes that matter. */
                ByteCount byteCount = 3;
                if ((ev.status & 0xF0) == 0xC0 || (ev.status & 0xF0) == 0xD0) {
                    byteCount = 2;
                }
                MIDITimeStamp ts = ev.sample_offset;
                if (ev.sample_offset >= inFrameCount) {
                    ts = inFrameCount > 0 ? (inFrameCount - 1) : 0;
                }
                pkt = append_or_flush_retry(
                    pktList, pkt, inst, inTimeStamp, ts, byteCount, data);
            }
            /* SysEx drain. MIDIPacketListAdd accepts the framed
             * (`0xF0` + inner + `0xF7`) byte stream as a single
             * packet of length `2 + len`; CoreMIDI carries SysEx
             * payloads of arbitrary size through one packet (no
             * 4-byte cap like AAX's `AAX_CMidiPacket`). We build
             * the framed bytes in `sysexFrameScratch` per event;
             * `MIDIPacketListAdd` copies them into the list
             * synchronously so reusing the scratch for the next
             * event is sound. If a single event exceeds the
             * packet-list size even on a freshly-flushed buffer,
             * skip it — truncating SysEx is corrupt. */
            for (uint32_t i = 0; pkt && i < sx_count; i++) {
                uint32_t delta = 0;
                const uint8_t *bytes = NULL;
                uint32_t len = 0;
                g_callbacks->output_sysex_at(inst->rustCtx, i,
                                              &delta, &bytes, &len);
                if (!bytes && len > 0) continue;
                uint32_t framedLen = len + 2;
                if (framedLen > inst->sysexFrameScratchSize) continue;
                inst->sysexFrameScratch[0] = 0xF0;
                if (len > 0) {
                    memcpy(inst->sysexFrameScratch + 1, bytes, len);
                }
                inst->sysexFrameScratch[1 + len] = 0xF7;
                MIDITimeStamp ts = delta;
                if (delta >= inFrameCount) {
                    ts = inFrameCount > 0 ? (inFrameCount - 1) : 0;
                }
                pkt = append_or_flush_retry(
                    pktList, pkt, inst, inTimeStamp, ts, framedLen,
                    inst->sysexFrameScratch);
            }

            /* Flush whatever's left in the list. The loop above
             * already flushed once per `add` failure, so the final
             * `pktList` may be empty — `numPackets == 0` is the
             * documented signal not to call the host callback. */
            if (pktList->numPackets > 0) {
                inst->midiOutputCallback(inst->midiOutputUserData,
                                         inTimeStamp, 0 /* outputIndex */,
                                         pktList);
            }
        }
    }

    // Copy our processed audio to the host's original buffers
    for (uint32_t c = 0; c < numOut && c < ioData->mNumberBuffers; c++) {
        if (hostBufs[c] && hostBufs[c] != inst->outputBuffers[c]) {
            memcpy(hostBufs[c], inst->outputBuffers[c], inFrameCount * sizeof(float));
            ioData->mBuffers[c].mData = hostBufs[c];
        } else {
            ioData->mBuffers[c].mData = inst->outputBuffers[c];
        }
        ioData->mBuffers[c].mDataByteSize = inFrameCount * sizeof(float);
    }

    return noErr;
}

// ---------------------------------------------------------------------------
// MIDI (instruments only)
// ---------------------------------------------------------------------------

static OSStatus au_v2_midi_event(void *self_, UInt32 status,
                                  UInt32 data1, UInt32 data2,
                                  UInt32 sampleOffset) {
    TruceAUv2 *inst = (TruceAUv2 *)self_;

    if (inst->midiCount >= 256) return noErr;

    AuMidiEvent *ev = &inst->midiBuffer[inst->midiCount++];
    ev->sample_offset = sampleOffset;
    ev->status = (uint8_t)status;
    ev->data1 = (uint8_t)data1;
    ev->data2 = (uint8_t)data2;
    ev->_pad = 0;
    return noErr;
}

// ---------------------------------------------------------------------------
// Property listeners / render notify (stubs)
// ---------------------------------------------------------------------------

static OSStatus au_v2_add_property_listener(void *self_, AudioUnitPropertyID prop,
                                             AudioUnitPropertyListenerProc proc,
                                             void *userData) {
    TruceAUv2 *inst = (TruceAUv2 *)self_;
    if (inst->listenerCount >= 32) return noErr;
    inst->listeners[inst->listenerCount].prop = prop;
    inst->listeners[inst->listenerCount].proc = proc;
    inst->listeners[inst->listenerCount].userData = userData;
    inst->listenerCount++;
    return noErr;
}

static OSStatus au_v2_remove_property_listener_with_data(void *self_, AudioUnitPropertyID prop,
                                                          AudioUnitPropertyListenerProc proc,
                                                          void *userData) {
    TruceAUv2 *inst = (TruceAUv2 *)self_;
    for (uint32_t i = 0; i < inst->listenerCount; i++) {
        if (inst->listeners[i].prop == prop && inst->listeners[i].proc == proc &&
            inst->listeners[i].userData == userData) {
            inst->listeners[i] = inst->listeners[--inst->listenerCount];
            break;
        }
    }
    return noErr;
}

static OSStatus au_v2_add_render_notify(void *self_, AURenderCallback proc, void *userData) {
    (void)self_; (void)proc; (void)userData;
    return noErr;
}

static OSStatus au_v2_remove_render_notify(void *self_, AURenderCallback proc, void *userData) {
    (void)self_; (void)proc; (void)userData;
    return noErr;
}

// ---------------------------------------------------------------------------
// Lookup — maps selectors to method function pointers
// ---------------------------------------------------------------------------

static AudioComponentMethod au_v2_lookup(SInt16 selector) {
    switch (selector) {
        case kAudioUnitInitializeSelect:
            return (AudioComponentMethod)au_v2_initialize;
        case kAudioUnitUninitializeSelect:
            return (AudioComponentMethod)au_v2_uninitialize;
        case kAudioUnitGetPropertyInfoSelect:
            return (AudioComponentMethod)au_v2_get_property_info;
        case kAudioUnitGetPropertySelect:
            return (AudioComponentMethod)au_v2_get_property;
        case kAudioUnitSetPropertySelect:
            return (AudioComponentMethod)au_v2_set_property;
        case kAudioUnitGetParameterSelect:
            return (AudioComponentMethod)au_v2_get_parameter;
        case kAudioUnitSetParameterSelect:
            return (AudioComponentMethod)au_v2_set_parameter;
        case kAudioUnitRenderSelect:
            return (AudioComponentMethod)au_v2_render;
        case kAudioUnitResetSelect:
            return (AudioComponentMethod)au_v2_reset;
        case kAudioUnitAddPropertyListenerSelect:
            return (AudioComponentMethod)au_v2_add_property_listener;
        case kAudioUnitRemovePropertyListenerWithUserDataSelect:
            return (AudioComponentMethod)au_v2_remove_property_listener_with_data;
        case kAudioUnitScheduleParametersSelect:
            return (AudioComponentMethod)au_v2_schedule_parameters;
        case kAudioUnitAddRenderNotifySelect:
            return (AudioComponentMethod)au_v2_add_render_notify;
        case kAudioUnitRemoveRenderNotifySelect:
            return (AudioComponentMethod)au_v2_remove_render_notify;
        case kMusicDeviceMIDIEventSelect:
            return is_instrument() ? (AudioComponentMethod)au_v2_midi_event : NULL;
        // Return NULL for optional selectors (system probes for capabilities)
        case 11: // kAudioUnitRemovePropertyListenerSelect (legacy)
        case 19: case 20: case 21: // misc component selectors
        case 513: case 514: // kAudioOutputUnitStart/Stop
        case 258: // kMusicDeviceSysExSelect
        case 259: // kMusicDevicePrepareInstrumentSelect
        case 260: // kMusicDeviceReleaseInstrumentSelect
        case 261: // kMusicDeviceStartNoteSelect
        case 262: // kMusicDeviceStopNoteSelect
        case 263: // kMusicDeviceMIDIEventListSelect
            return NULL;
        default:
            return NULL;
    }
}

// ---------------------------------------------------------------------------
// Factory function — exported symbol, referenced by Info.plist factoryFunction.
// Returns an AudioComponentPlugInInterface* (AU v2 interface). The
// real definition is in the consumer cdylib via the Rust `export_au!`
// macro — it forwards to `truce_au_v2_factory_bridge` defined below.
// ---------------------------------------------------------------------------

static void *truce_au_v2_factory(const AudioComponentDescription *desc) {
    (void)desc;
    TruceAUv2 *inst = (TruceAUv2 *)calloc(1, sizeof(TruceAUv2));
    if (!inst) return NULL;

    inst->interface.Open = au_v2_open;
    inst->interface.Close = au_v2_close;
    inst->interface.Lookup = au_v2_lookup;
    inst->interface.reserved = NULL;

    /* 132 KiB: matches truce_core::SYSEX_POOL_PREALLOC (128 KiB)
     * plus headroom for per-packet headers (≈14 B × ≤256 events).
     * Heap-allocated to keep the TruceAUv2 struct itself small;
     * never reallocated after this point. */
    inst->sysexPacketBufSize = 132 * 1024;
    inst->sysexPacketBuf = (Byte *)malloc(inst->sysexPacketBufSize);
    inst->sysexFrameScratchSize = 128 * 1024 + 2; /* SYSEX_POOL_PREALLOC + framing */
    inst->sysexFrameScratch = (Byte *)malloc(inst->sysexFrameScratchSize);
    if (!inst->sysexPacketBuf || !inst->sysexFrameScratch) {
        free(inst->sysexPacketBuf);
        free(inst->sysexFrameScratch);
        free(inst);
        return NULL;
    }

    return &inst->interface;
}

// Called from the Rust-exported TruceAUFactory symbol.
void *truce_au_v2_factory_bridge(const void *desc) {
    return truce_au_v2_factory((const AudioComponentDescription *)desc);
}
