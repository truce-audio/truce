/**
 * VST2 types — clean-room definitions of the AEffect interface.
 * These are the public C ABI types that all VST2 hosts expect.
 * No Steinberg SDK headers are used.
 */

#ifndef VST2_TYPES_H
#define VST2_TYPES_H

#include <stdint.h>

/* VST2 magic number: 'VstP' */
#define kVstMagic 0x56737450

/* AEffect flags */
#define effFlagsHasEditor       (1 << 0)
#define effFlagsCanReplacing    (1 << 4)
#define effFlagsProgramChunks   (1 << 5)
#define effFlagsIsSynth         (1 << 8)
#define effFlagsNoSoundInStop   (1 << 9)
#define effFlagsCanDoubleReplacing (1 << 12)

/* Dispatcher opcodes */
#define effOpen             0
#define effClose            1
#define effSetProgram       2
#define effGetProgram       3
#define effGetProgramName   5
#define effGetParamLabel    6
#define effGetParamDisplay  7
#define effGetParamName     8
#define effSetSampleRate    10
#define effSetBlockSize     11
#define effMainsChanged     12
#define effEditGetRect      13
#define effEditOpen         14
#define effEditClose        15
#define effEditIdle         19
#define effGetChunk         23
#define effSetChunk         24
#define effProcessEvents    25
#define effCanBeAutomated   26
#define effGetProductString 48
#define effGetVendorString  47
#define effGetVendorVersion 49
#define effCanDo            51
#define effGetTailSize      52
#define effGetEffectName    45
#define effBeginSetProgram  67
#define effEndSetProgram    68

/* audioMaster opcodes (host callbacks) */
#define audioMasterAutomate 0
#define audioMasterVersion  1
#define audioMasterGetTime  7
#define audioMasterBeginEdit 43
#define audioMasterEndEdit   44

/* audioMasterGetTime request flags (which fields the plugin wants filled) */
#define kVstNanosValid       (1 << 8)
#define kVstPpqPosValid      (1 << 9)
#define kVstTempoValid       (1 << 10)
#define kVstBarsValid        (1 << 11)
#define kVstCyclePosValid    (1 << 12)
#define kVstTimeSigValid     (1 << 13)
/* Transport state flags returned in VstTimeInfo::flags */
#define kVstTransportChanged     (1 << 0)
#define kVstTransportPlaying     (1 << 1)
#define kVstTransportCycleActive (1 << 2)
#define kVstTransportRecording   (1 << 3)

/* VstEvent types */
#define kVstMidiType 1

typedef intptr_t VstIntPtr;

/* Forward declaration */
typedef struct AEffect AEffect;

/* Host callback */
typedef VstIntPtr (*audioMasterCallback)(AEffect* effect, int32_t opcode,
    int32_t index, VstIntPtr value, void* ptr, float opt);

/* The main plugin struct — returned by VSTPluginMain */
struct AEffect {
    int32_t magic;                  /* Must be kVstMagic */

    VstIntPtr (*dispatcher)(AEffect*, int32_t opcode, int32_t index,
                            VstIntPtr value, void* ptr, float opt);

    /* Deprecated accumulating process — unused, set to NULL */
    void (*process)(AEffect*, float** inputs, float** outputs,
                    int32_t sampleFrames);

    void (*setParameter)(AEffect*, int32_t index, float value);
    float (*getParameter)(AEffect*, int32_t index);

    int32_t numPrograms;
    int32_t numParams;
    int32_t numInputs;
    int32_t numOutputs;
    int32_t flags;

    void* resvd1;               /* Host-specific, do not use */
    void* resvd2;

    int32_t initialDelay;       /* Latency in samples */

    int32_t _realQualities;     /* Deprecated */
    int32_t _offQualities;      /* Deprecated */
    float   _ioRatio;           /* Deprecated */

    void* object;               /* Plugin instance pointer */
    void* user;                 /* User pointer (unused) */

    int32_t uniqueID;           /* FourCC plugin identifier */
    int32_t version;

    void (*processReplacing)(AEffect*, float** inputs, float** outputs,
                             int32_t sampleFrames);

    void (*processDoubleReplacing)(AEffect*, double** inputs,
                                   double** outputs, int32_t sampleFrames);

    char future[56];            /* Reserved */
};

/* VstTimeInfo — host time + transport state.
 * Memory layout matches the VST 2.4 SDK so we can cast the audioMasterGetTime
 * return directly. Clean-room definition, no Steinberg headers. */
typedef struct {
    double sample_pos;
    double sample_rate;
    double nano_seconds;
    double ppq_pos;             /* position in beats */
    double tempo;               /* BPM */
    double bar_start_pos;       /* beats at start of current bar */
    double cycle_start_pos;     /* loop start, in beats */
    double cycle_end_pos;       /* loop end, in beats */
    int32_t time_sig_num;
    int32_t time_sig_den;
    int32_t smpte_offset;
    int32_t smpte_frame_rate;
    int32_t samples_to_next_clock;
    int32_t flags;              /* kVstTransport... and kVst...Valid bits */
} VstTimeInfo;

/* Compact transport snapshot passed across the FFI boundary so Rust
 * does not need to know VstTimeInfo's layout. */
typedef struct {
    int32_t valid;              /* 1 = host returned time info, 0 otherwise */
    int32_t playing;
    int32_t recording;
    int32_t loop_active;
    int32_t time_sig_num;       /* 0 if host did not report */
    int32_t time_sig_den;
    double tempo;               /* 0 if host did not report */
    double position_samples;
    double position_beats;      /* 0 if host did not report */
    double bar_start_beats;
    double loop_start_beats;
    double loop_end_beats;
} Vst2TransportSnapshot;

/* MIDI event */
typedef struct {
    int32_t type;               /* kVstMidiType = 1 */
    int32_t byteSize;           /* sizeof(VstMidiEvent) */
    int32_t deltaFrames;        /* Sample offset */
    int32_t flags;
    int32_t noteLength;
    int32_t noteOffset;
    char midiData[4];           /* status, data1, data2, reserved */
    char detune;
    char noteOffVelocity;
    char reserved1;
    char reserved2;
} VstMidiEvent;

/* Event container passed to effProcessEvents */
typedef struct {
    int32_t type;
    int32_t byteSize;
    int32_t deltaFrames;
    int32_t flags;
    char data[16];              /* Enough for any event type */
} VstEvent;

typedef struct {
    int32_t numEvents;
    VstIntPtr reserved;
    VstEvent* events[2];        /* Variable length — [2] for alignment */
} VstEvents;

/* Truce callback types (Rust → C boundary) */
typedef struct {
    uint32_t id;
    const char* name;
    double min;
    double max;
    double default_value;
    uint32_t step_count;
    const char* unit;
    const char* group;
} Vst2ParamDescriptor;

typedef struct {
    uint8_t component_type[4];
    uint8_t component_subtype[4];
    const char* name;
    const char* vendor;
    uint32_t version;
    uint32_t num_inputs;
    uint32_t num_outputs;
} Vst2PluginDescriptor;

typedef struct {
    uint32_t delta_frames;
    uint8_t status;
    uint8_t data1;
    uint8_t data2;
    uint8_t _pad;
} Vst2MidiEventCompact;

typedef struct {
    void* (*create)(void);
    void  (*destroy)(void* ctx);
    void  (*reset)(void* ctx, double sample_rate, uint32_t max_frames);
    void  (*process)(void* ctx,
                     const float** inputs, float** outputs,
                     uint32_t num_input_channels, uint32_t num_output_channels,
                     uint32_t num_frames,
                     const Vst2MidiEventCompact* events, uint32_t num_events);
    uint32_t (*param_count)(void* ctx);
    void     (*param_get_descriptor)(void* ctx, uint32_t index, Vst2ParamDescriptor* out);
    double   (*param_get_value)(void* ctx, uint32_t id);
    void     (*param_set_value)(void* ctx, uint32_t id, double value);
    uint32_t (*param_format_value)(void* ctx, uint32_t id, double value,
                                    char* out, uint32_t out_len);
    void (*state_save)(void* ctx, uint8_t** out_data, uint32_t* out_len);
    void (*state_load)(void* ctx, const uint8_t* data, uint32_t len);
    void (*state_free)(uint8_t* data, uint32_t len);
    /* Latency + tail */
    uint32_t (*get_latency)(void* ctx);
    uint32_t (*get_tail)(void* ctx);
    /* Host notification */
    void (*set_effect_ptr)(void* ctx, void* effect);
    /* GUI */
    int32_t (*gui_has_editor)(void* ctx);
    void (*gui_get_size)(void* ctx, uint32_t* w, uint32_t* h);
    void (*gui_open)(void* ctx, void* parent);
    void (*gui_close)(void* ctx);
} Vst2Callbacks;

/* Globals set by Rust registration */
extern const Vst2PluginDescriptor* g_vst2_descriptor;
extern const Vst2Callbacks* g_vst2_callbacks;
extern const Vst2ParamDescriptor* g_vst2_params;
extern uint32_t g_vst2_num_params;

/* Rust-side init */
extern void truce_vst2_init(void);

/* FourCC helper */
#define FOURCC(b) (((int32_t)(b)[0] << 24) | ((int32_t)(b)[1] << 16) | \
                   ((int32_t)(b)[2] << 8)  | (int32_t)(b)[3])

#endif /* VST2_TYPES_H */
