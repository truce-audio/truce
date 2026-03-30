/**
 * Truce VST3 C++ shim.
 *
 * Implements the VST3 COM interfaces (IComponent, IAudioProcessor,
 * IEditController, IPluginFactory) in real C++ so the vtable layout
 * matches what hosts expect. All plugin logic is delegated to Rust
 * via C FFI callbacks.
 */

#include <cstdint>
#include <cstdio>
#include <cstring>
#include <cstdlib>
#include <atomic>
#include <new>

#define I(x) static_cast<int8_t>(x)

// ---------------------------------------------------------------------------
// Minimal VST3 type definitions (no SDK dependency)
// ---------------------------------------------------------------------------

typedef int32_t tresult;
typedef int32_t int32;
typedef uint32_t uint32;
typedef int16_t char16;
typedef int8_t int8;
typedef char char8;
typedef int8_t TBool;
typedef uint64_t TSize;

typedef int8_t TUID[16];
typedef const char* FIDString;

static const tresult kResultOk = 0;
static const tresult kResultFalse = 1;
static const tresult kInvalidArgument = 5;
static const tresult kNotImplemented = 2;

// IIDs from the official Steinberg VST3 SDK (via vst3 crate bindings)
static const TUID FUnknown_iid        = {I(0x00),I(0x00),I(0x00),I(0x00),I(0x00),I(0x00),I(0x00),I(0x00),I(0xC0),I(0x00),I(0x00),I(0x00),I(0x00),I(0x00),I(0x00),I(0x46)};
static const TUID IPluginBase_iid     = {I(0x22),I(0x88),I(0x8D),I(0xDB),I(0x15),I(0x6E),I(0x45),I(0xAE),I(0x83),I(0x58),I(0xB3),I(0x48),I(0x08),I(0x19),I(0x06),I(0x25)};
static const TUID IComponent_iid      = {I(0xE8),I(0x31),I(0xFF),I(0x31),I(0xF2),I(0xD5),I(0x43),I(0x01),I(0x92),I(0x8E),I(0xBB),I(0xEE),I(0x25),I(0x69),I(0x78),I(0x02)};
static const TUID IAudioProcessor_iid = {I(0x42),I(0x04),I(0x3F),I(0x99),I(0xB7),I(0xDA),I(0x45),I(0x3C),I(0xA5),I(0x69),I(0xE7),I(0x9D),I(0x9A),I(0xAE),I(0xC3),I(0x3D)};
static const TUID IEditController_iid = {I(0xDC),I(0xD7),I(0xBB),I(0xE3),I(0x77),I(0x42),I(0x44),I(0x8D),I(0xA8),I(0x74),I(0xAA),I(0xCC),I(0x97),I(0x9C),I(0x75),I(0x9E)};
static const TUID IPluginFactory_iid  = {I(0x7A),I(0x4D),I(0x81),I(0x1C),I(0x52),I(0x11),I(0x4A),I(0x1F),I(0xAE),I(0xD9),I(0xD2),I(0xEE),I(0x0B),I(0x43),I(0xBF),I(0x9F)};
static const TUID IPlugView_iid       = {I(0x5B),I(0xC3),I(0x25),I(0x07),I(0xD0),I(0x60),I(0x49),I(0xEA),I(0xA6),I(0x15),I(0x1B),I(0x52),I(0x2B),I(0x75),I(0x5B),I(0x29)};
static const TUID IEditControllerHostEditing_iid = {I(0x0F),I(0x19),I(0x47),I(0x81),I(0x8D),I(0x98),I(0x4A),I(0xDA),I(0xBB),I(0xA0),I(0xC1),I(0xEF),I(0xC0),I(0x11),I(0xD8),I(0xD0)};
static const TUID IProcessContextRequirements_iid = {I(0x2A),I(0x65),I(0x43),I(0x03),I(0xEF),I(0x76),I(0x4E),I(0x3C),I(0xA8),I(0xE8),I(0xC6),I(0xF3),I(0xDB),I(0xAE),I(0x0F),I(0x77)};
static const TUID IUnitInfo_iid = {I(0x3D),I(0x4B),I(0xD6),I(0xB5),I(0x91),I(0x3A),I(0x4F),I(0xD2),I(0xA8),I(0x86),I(0xE7),I(0x68),I(0xA5),I(0x33),I(0x2E),I(0x1F)};

static const char* kPlatformTypeNSView = "NSView";

// Media types & bus directions
enum { kAudio = 0, kEvent = 1 };
enum { kInput = 0, kOutput = 1 };
enum { kMain = 0, kAux = 1 };
enum { kSample32 = 0 };

static bool iid_equal(const TUID a, const TUID b) {
    return memcmp(a, b, 16) == 0;
}

static void str_to_char16(char16* dst, const char* src, int maxLen) {
    int i = 0;
    for (; src[i] && i < maxLen - 1; i++) dst[i] = (char16)src[i];
    dst[i] = 0;
}

static void str_to_char8(char8* dst, const char* src, int maxLen) {
    int i = 0;
    for (; src[i] && i < maxLen - 1; i++) dst[i] = src[i];
    dst[i] = 0;
}

// ---------------------------------------------------------------------------
// FFI types (must match Rust ffi.rs)
// ---------------------------------------------------------------------------

struct Vst3PluginDescriptor {
    const char* name;
    const char* vendor;
    const char* url;
    const char* email;
    const char* version;
    uint8_t cid[16];
    const char* category;
    const char* subcategories;
    uint32_t num_inputs;
    uint32_t num_outputs;
};

struct Vst3ParamDescriptor {
    uint32_t id;
    const char* name;
    const char* short_name;
    const char* units;
    double min;
    double max;
    double default_normalized;
    int32_t step_count;
    int32_t flags;
    const char* group;
};

struct Vst3MidiEvent {
    uint32_t sample_offset;
    uint8_t status;
    uint8_t data1;
    uint8_t data2;
    uint8_t _pad;
};

struct Vst3Transport {
    int32_t playing;
    int32_t recording;
    double tempo;
    int32_t time_sig_num;
    int32_t time_sig_den;
    double position_samples;
    double position_beats;
    double bar_start_beats;
    double cycle_start_beats;
    double cycle_end_beats;
    int32_t cycle_active;
};

struct Vst3ParamChange {
    uint32_t id;
    int32_t sample_offset;
    double value; // plain value
};

struct Vst3Callbacks {
    void* (*create)();
    void (*destroy)(void*);
    void (*reset)(void*, double, uint32_t);
    void (*process)(void*, const float**, float**, uint32_t, uint32_t, uint32_t,
                    const Vst3MidiEvent*, uint32_t,
                    const Vst3Transport*, const Vst3ParamChange*, uint32_t);
    uint32_t (*param_count)(void*);
    void (*param_get_descriptor)(void*, uint32_t, Vst3ParamDescriptor*);
    double (*param_get_value)(void*, uint32_t);
    void (*param_set_value)(void*, uint32_t, double);
    double (*param_normalize)(void*, uint32_t, double);
    double (*param_denormalize)(void*, uint32_t, double);
    uint32_t (*param_format)(void*, uint32_t, double, char*, uint32_t);
    void (*state_save)(void*, uint8_t**, uint32_t*);
    void (*state_load)(void*, const uint8_t*, uint32_t);
    void (*state_free)(uint8_t*, uint32_t);
    // Latency + tail
    uint32_t (*get_latency)(void*);
    uint32_t (*get_tail)(void*);
    // Output events
    uint32_t (*get_output_event_count)(void*);
    void (*get_output_event)(void*, uint32_t, Vst3MidiEvent*);
    // GUI
    int32_t (*gui_has_editor)(void*);
    void (*gui_get_size)(void*, uint32_t*, uint32_t*);
    void (*gui_open)(void*, void*);
    void (*gui_close)(void*);
};

// ---------------------------------------------------------------------------
// Global registration (set by Rust)
// ---------------------------------------------------------------------------

static const Vst3PluginDescriptor* g_desc = nullptr;
static const Vst3Callbacks* g_cb = nullptr;
static const Vst3ParamDescriptor* g_params = nullptr;
static uint32_t g_num_params = 0;

// Unit info (parameter groups) — built at registration time
static const int kMaxUnits = 64;
struct UnitEntry { int32 id; int32 parentId; char name[128]; };
static UnitEntry g_units[kMaxUnits];
static int g_num_units = 0;
// Maps param index → unit ID
static int32 g_param_unit_id[1024];

static void build_unit_map() {
    // Unit 0 = root (always exists)
    g_num_units = 1;
    g_units[0].id = 0;
    g_units[0].parentId = -1; // kNoParentUnitId
    strncpy(g_units[0].name, "Root", sizeof(g_units[0].name));

    for (uint32_t i = 0; i < g_num_params && i < 1024; i++) {
        const char* group = g_params[i].group;
        if (!group || group[0] == 0) {
            g_param_unit_id[i] = 0; // root unit
            continue;
        }
        // Find or create unit for this group
        int32 unitId = -1;
        for (int u = 1; u < g_num_units; u++) {
            if (strcmp(g_units[u].name, group) == 0) {
                unitId = g_units[u].id;
                break;
            }
        }
        if (unitId < 0 && g_num_units < kMaxUnits) {
            unitId = g_num_units;
            g_units[g_num_units].id = unitId;
            g_units[g_num_units].parentId = 0; // parent = root
            strncpy(g_units[g_num_units].name, group, sizeof(g_units[0].name));
            g_num_units++;
        }
        g_param_unit_id[i] = (unitId >= 0) ? unitId : 0;
    }
}

// ---------------------------------------------------------------------------
// VST3 structs (matching SDK layout)
// ---------------------------------------------------------------------------

struct ProcessSetup {
    int32 processMode;
    int32 symbolicSampleSize;
    int32 maxSamplesPerBlock;
    double sampleRate;
};

struct AudioBusBuffers {
    int32 numChannels;
    uint64_t silenceFlags;
    union { float** channelBuffers32; double** channelBuffers64; };
};

// VST3 ProcessContext (transport info from host)
struct Vst3ProcessContext {
    uint32 state;                 // combination of StatesAndFlags
    double sampleRate;
    double projectTimeSamples;    // project time in samples
    double systemTime;            // system time in nanoseconds
    double continousTimeSamples;
    double projectTimeMusic;      // musical position in quarter notes
    double barPositionMusic;      // last bar start position in quarter notes
    double cycleStartMusic;       // cycle start in quarter notes
    double cycleEndMusic;         // cycle end in quarter notes
    double tempo;                 // tempo in BPM
    int32 timeSigNumerator;
    int32 timeSigDenominator;
    // chord + key signature fields follow but we don't need them
};

// ProcessContext state flags
enum {
    kPlaying          = 1 << 1,
    kRecording        = 1 << 3,
    kTempoValid       = 1 << 10,
    kTimeSigValid     = 1 << 11,
    kProjectTimeMusicValid = 1 << 9,
    kBarPositionValid = 1 << 13,
    kCycleValid       = 1 << 12,
};

struct ProcessData {
    int32 processMode;
    int32 symbolicSampleSize;
    int32 numSamples;
    int32 numInputs;
    int32 numOutputs;
    AudioBusBuffers* inputs;
    AudioBusBuffers* outputs;
    void* inputParameterChanges;   // IParameterChanges*
    void* outputParameterChanges;
    void* inputEvents;             // IEventList*
    void* outputEvents;
    Vst3ProcessContext* processContext;
};

struct BusInfo {
    int32 mediaType;
    int32 direction;
    int32 channelCount;
    char16 name[128];
    int32 busType;
    uint32 flags;
};

struct RoutingInfo { int32 mediaType; int32 busIndex; int32 channel; };

struct ParameterInfo {
    uint32 id;
    char16 title[128];
    char16 shortTitle[128];
    char16 units[128];
    int32 stepCount;
    double defaultNormalizedValue;
    int32 unitId;
    int32 flags;
};

struct PFactoryInfo {
    char8 vendor[64];
    char8 url[256];
    char8 email[128];
    int32 flags;
};

struct PClassInfo {
    TUID cid;
    int32 cardinality;
    char8 category[32];
    char8 name[64];
};

// ---------------------------------------------------------------------------
// Component: IComponent + IAudioProcessor + IEditController
// ---------------------------------------------------------------------------

struct TruceComponentCOM; // forward declaration
class TruceComponent;     // forward declaration for ctx mapping

// Global ctx→component mapping for extern "C" host-notification callbacks
static constexpr int kMaxInstances = 64;
static void* g_ctx_map_key[kMaxInstances] = {};
static TruceComponent* g_ctx_map_comp[kMaxInstances] = {};

class TruceComponent {
    std::atomic<int32> refCount{1};
    void* ctx;
    double sampleRate;
    uint32_t maxFrames;
public:
    void* componentHandler;  // IComponentHandler*, stored with addRef
    bool inPerformEdit;       // feedback guard: skip setParamNormalized during performEdit

    TruceComponent() : ctx(nullptr), sampleRate(44100), maxFrames(1024),
                       componentHandler(nullptr), inPerformEdit(false) {
        if (g_cb) {
            ctx = g_cb->create();
            if (ctx) {
                for (int i = 0; i < kMaxInstances; i++) {
                    if (!g_ctx_map_key[i]) {
                        g_ctx_map_key[i] = ctx;
                        g_ctx_map_comp[i] = this;
                        break;
                    }
                }
            }
        }
    }

    ~TruceComponent() {
        if (componentHandler) {
            auto release_fn = (uint32 (*)(void*))(*(void***)componentHandler)[2];
            release_fn(componentHandler);
        }
        if (ctx) {
            for (int i = 0; i < kMaxInstances; i++) {
                if (g_ctx_map_key[i] == ctx) {
                    g_ctx_map_key[i] = nullptr;
                    g_ctx_map_comp[i] = nullptr;
                    break;
                }
            }
        }
        if (g_cb && ctx) g_cb->destroy(ctx);
    }

    // --- FUnknown ---
    // Note: queryInterface returns pointers into the containing TruceComponentCOM,
    // NOT into this TruceComponent. The caller (vtable functions) must pass the
    // correct COM wrapper pointer.

    // Defined after TruceComponentCOM (needs complete type)
    tresult queryInterface(void* comBase, const TUID iid, void** obj);

    uint32 addRef() { return ++refCount; }
    uint32 release() { return --refCount; }

    // --- IPluginBase ---

    tresult initialize(void* /*context*/) {
        if (g_cb && ctx) {
            // initialize is a no-op for now
        }
        return kResultOk;
    }

    tresult terminate() { return kResultOk; }

    // --- IComponent ---

    tresult getControllerClassId(TUID /*classId*/) { return kNotImplemented; }
    tresult setIoMode(int32) { return kResultOk; }

    int32 getBusCount(int32 type, int32 dir) {
        if (!g_desc) return 0;
        if (type == kAudio) {
            return (dir == kInput) ? (g_desc->num_inputs > 0 ? 1 : 0) : 1;
        }
        if (type == kEvent && dir == kInput && g_desc->num_inputs == 0) {
            return 1; // instrument: one event input bus
        }
        return 0;
    }

    tresult getBusInfo(int32 type, int32 dir, int32 index, BusInfo* bus) {
        if (!bus || !g_desc) return kInvalidArgument;
        if (type == kEvent && dir == kInput && index == 0 && g_desc->num_inputs == 0) {
            bus->mediaType = kEvent; bus->direction = kInput; bus->channelCount = 1;
            str_to_char16(bus->name, "Event In", 128);
            bus->busType = kMain; bus->flags = 1; // kDefaultActive
            return kResultOk;
        }
        if (type != kAudio) return kInvalidArgument;
        if (dir == kInput && index == 0 && g_desc->num_inputs > 0) {
            bus->mediaType = kAudio; bus->direction = kInput;
            bus->channelCount = g_desc->num_inputs;
            str_to_char16(bus->name, "Input", 128);
            bus->busType = kMain; bus->flags = 1;
            return kResultOk;
        }
        if (dir == kOutput && index == 0) {
            bus->mediaType = kAudio; bus->direction = kOutput;
            bus->channelCount = g_desc->num_outputs;
            str_to_char16(bus->name, "Output", 128);
            bus->busType = kMain; bus->flags = 1;
            return kResultOk;
        }
        return kInvalidArgument;
    }

    tresult getRoutingInfo(RoutingInfo*, RoutingInfo*) { return kNotImplemented; }
    tresult activateBus(int32, int32, int32, TBool) { return kResultOk; }

    tresult setActive(TBool state) {
        if (state && g_cb && ctx) {
            g_cb->reset(ctx, sampleRate, maxFrames);
        }
        return kResultOk;
    }

    tresult setState(void* stream) {
        if (!stream || !g_cb || !ctx) return kResultFalse;
        // IBStream vtable: queryInterface, addRef, release, read, write, seek, tell
        struct IBStreamVtbl {
            tresult (*queryInterface)(void*, const TUID, void**);
            uint32  (*addRef)(void*);
            uint32  (*release)(void*);
            tresult (*read)(void*, void*, int32, int32*);
            tresult (*write)(void*, void*, int32, int32*);
            tresult (*seek)(void*, int64_t, int32, int64_t*);
            tresult (*tell)(void*, int64_t*);
        };
        auto* vtbl = *reinterpret_cast<IBStreamVtbl**>(stream);

        // Read all available data from the stream
        uint8_t buf[4096];
        uint8_t* data = nullptr;
        int32 total = 0;
        for (;;) {
            int32 bytesRead = 0;
            tresult r = vtbl->read(stream, buf, (int32)sizeof(buf), &bytesRead);
            if (bytesRead <= 0) break;
            auto* prev = data;
            data = (uint8_t*)realloc(data, total + bytesRead);
            if (!data) { free(prev); return kResultFalse; }
            memcpy(data + total, buf, bytesRead);
            total += bytesRead;
            if (r != kResultOk) break;
        }
        if (data && total > 0) {
            g_cb->state_load(ctx, data, (uint32_t)total);
        }
        free(data);
        return kResultOk;
    }

    tresult getState(void* stream) {
        if (!stream || !g_cb || !ctx) return kResultFalse;
        struct IBStreamVtbl {
            tresult (*queryInterface)(void*, const TUID, void**);
            uint32  (*addRef)(void*);
            uint32  (*release)(void*);
            tresult (*read)(void*, void*, int32, int32*);
            tresult (*write)(void*, void*, int32, int32*);
            tresult (*seek)(void*, int64_t, int32, int64_t*);
            tresult (*tell)(void*, int64_t*);
        };
        auto* vtbl = *reinterpret_cast<IBStreamVtbl**>(stream);

        uint8_t* blob = nullptr;
        uint32_t len = 0;
        g_cb->state_save(ctx, &blob, &len);
        if (blob && len > 0) {
            int32 written = 0;
            vtbl->write(stream, blob, (int32)len, &written);
            g_cb->state_free(blob, len);
            if (written != (int32)len) return kResultFalse;
        }
        return kResultOk;
    }

    // --- IAudioProcessor ---

    tresult setBusArrangements(uint64_t*, int32, uint64_t*, int32) { return kResultOk; }

    tresult getBusArrangement(int32 dir, int32 index, uint64_t* arr) {
        if (!arr || !g_desc) return kInvalidArgument;
        uint32_t ch = (dir == kInput) ? g_desc->num_inputs : g_desc->num_outputs;
        if (index != 0) return kInvalidArgument;
        // Stereo = 0x3, Mono = 0x1
        *arr = (ch >= 2) ? 0x3 : (ch == 1 ? 0x1 : 0);
        return kResultOk;
    }

    tresult canProcessSampleSize(int32 symbolicSize) {
        return (symbolicSize == kSample32) ? kResultOk : kResultFalse;
    }

    uint32 getLatencySamples() {
        return (g_cb && ctx && g_cb->get_latency) ? g_cb->get_latency(ctx) : 0;
    }

    tresult setupProcessing(ProcessSetup* setup) {
        if (!setup) return kInvalidArgument;
        sampleRate = setup->sampleRate;
        maxFrames = setup->maxSamplesPerBlock;
        return kResultOk;
    }

    tresult setProcessing(TBool) { return kResultOk; }

    tresult process(ProcessData* data) {
        if (!data || !g_cb || !ctx) return kResultOk;

        // Collect ALL param change points (sample-accurate automation)
        Vst3ParamChange paramChanges[512];
        uint32_t numParamChanges = 0;

        if (data->inputParameterChanges) {
            auto** pcVtbl = (void**)*(void**)data->inputParameterChanges;
            auto getParamCount = (int32 (*)(void*))pcVtbl[3];
            auto getParamData  = (void* (*)(void*, int32))pcVtbl[4];
            int32 numChanges = getParamCount(data->inputParameterChanges);
            for (int32 i = 0; i < numChanges; i++) {
                void* queue = getParamData(data->inputParameterChanges, i);
                if (!queue) continue;
                auto** qVtbl = (void**)*(void**)queue;
                auto getParamId   = (uint32 (*)(void*))qVtbl[3];
                auto getPointCnt  = (int32 (*)(void*))qVtbl[4];
                auto getPoint     = (tresult (*)(void*, int32, int32*, double*))qVtbl[5];
                uint32 paramId = getParamId(queue);
                int32 numPoints = getPointCnt(queue);
                for (int32 j = 0; j < numPoints && numParamChanges < 512; j++) {
                    int32 sampleOffset;
                    double value;
                    if (getPoint(queue, j, &sampleOffset, &value) == kResultOk) {
                        double plain = g_cb->param_denormalize(ctx, paramId, value);
                        paramChanges[numParamChanges].id = paramId;
                        paramChanges[numParamChanges].sample_offset = sampleOffset;
                        paramChanges[numParamChanges].value = plain;
                        numParamChanges++;
                        // Also set the atomic value for the last point
                        if (j == numPoints - 1)
                            g_cb->param_set_value(ctx, paramId, plain);
                    }
                }
            }
        }

        // Extract transport info
        Vst3Transport transport = {};
        Vst3Transport* transportPtr = nullptr;
        if (data->processContext) {
            auto* pc = data->processContext;
            transport.playing = (pc->state & kPlaying) ? 1 : 0;
            transport.recording = (pc->state & kRecording) ? 1 : 0;
            transport.tempo = (pc->state & kTempoValid) ? pc->tempo : 120.0;
            transport.time_sig_num = (pc->state & kTimeSigValid) ? pc->timeSigNumerator : 4;
            transport.time_sig_den = (pc->state & kTimeSigValid) ? pc->timeSigDenominator : 4;
            transport.position_samples = pc->projectTimeSamples;
            transport.position_beats = (pc->state & kProjectTimeMusicValid) ? pc->projectTimeMusic : 0.0;
            transport.bar_start_beats = (pc->state & kBarPositionValid) ? pc->barPositionMusic : 0.0;
            transport.cycle_start_beats = (pc->state & kCycleValid) ? pc->cycleStartMusic : 0.0;
            transport.cycle_end_beats = (pc->state & kCycleValid) ? pc->cycleEndMusic : 0.0;
            transport.cycle_active = (pc->state & kCycleValid) ? 1 : 0;
            transportPtr = &transport;
        }

        int32 numFrames = data->numSamples;
        if (numFrames == 0) return kResultOk;

        // Collect input/output channel pointers
        const float* inPtrs[32] = {};
        float* outPtrs[32] = {};
        uint32_t numIn = 0, numOut = 0;

        if (data->numInputs > 0 && data->inputs) {
            auto& bus = data->inputs[0];
            numIn = bus.numChannels;
            for (int32 c = 0; c < bus.numChannels && c < 32; c++)
                inPtrs[c] = bus.channelBuffers32[c];
        }
        if (data->numOutputs > 0 && data->outputs) {
            auto& bus = data->outputs[0];
            numOut = bus.numChannels;
            for (int32 c = 0; c < bus.numChannels && c < 32; c++)
                outPtrs[c] = bus.channelBuffers32[c];
        }

        // Copy input to output for in-place processing
        uint32_t copyChannels = (numIn < numOut) ? numIn : numOut;
        for (uint32_t c = 0; c < copyChannels; c++) {
            if (inPtrs[c] && outPtrs[c] && inPtrs[c] != outPtrs[c])
                memcpy(outPtrs[c], inPtrs[c], numFrames * sizeof(float));
        }

        // Convert VST3 input events (note on/off) to Vst3MidiEvent
        Vst3MidiEvent midiEvents[256];
        uint32_t numMidi = 0;

        if (data->inputEvents) {
            // IEventList vtable: qi, addRef, release, getEventCount, getEvent, addEvent
            struct IEventListVtbl {
                tresult (*qi)(void*, const TUID, void**);
                uint32 (*addRef)(void*);
                uint32 (*release)(void*);
                int32 (*getEventCount)(void*);
                tresult (*getEvent)(void*, int32, void*);
                tresult (*addEvent)(void*, void*);
            };
            struct { IEventListVtbl* vtbl; } *eventList =
                (decltype(eventList))data->inputEvents;

            // VST3 Event struct layout (must match SDK exactly)
            // The union requires 8-byte alignment due to NoteExpressionValueEvent containing a double.
            struct Vst3Event {
                int32 busIndex;          // offset 0
                int32 sampleOffset;      // offset 4
                double ppqPosition;      // offset 8
                uint16_t flags;          // offset 16
                uint16_t type;           // offset 18
                // 4 bytes padding here (union is 8-byte aligned)
                union {
                    struct { int16_t channel; int16_t pitch; float tuning; float velocity; int32 length; int32 noteId; } noteOn;
                    struct { int16_t channel; int16_t pitch; float velocity; int32 noteId; float tuning; } noteOff;
                    struct { int16_t channel; int16_t pitch; float pressure; int32 noteId; } polyPressure;
                    struct { int32 typeId; int32 noteId; double value; } noteExpressionValue; // forces 8-byte alignment
                };
            };

            int32 eventCount = eventList->vtbl->getEventCount(eventList);
            for (int32 i = 0; i < eventCount && numMidi < 256; i++) {
                Vst3Event ev = {};
                if (eventList->vtbl->getEvent(eventList, i, &ev) != kResultOk)
                    continue;

                switch (ev.type) {
                    case 0: // kNoteOnEvent
                        midiEvents[numMidi].sample_offset = ev.sampleOffset;
                        midiEvents[numMidi].status = 0x90 | (ev.noteOn.channel & 0x0F);
                        midiEvents[numMidi].data1 = ev.noteOn.pitch & 0x7F;
                        midiEvents[numMidi].data2 = (uint8_t)(ev.noteOn.velocity * 127.0f);
                        midiEvents[numMidi]._pad = 0;
                        numMidi++;
                        break;
                    case 1: // kNoteOffEvent
                        midiEvents[numMidi].sample_offset = ev.sampleOffset;
                        midiEvents[numMidi].status = 0x80 | (ev.noteOff.channel & 0x0F);
                        midiEvents[numMidi].data1 = ev.noteOff.pitch & 0x7F;
                        midiEvents[numMidi].data2 = (uint8_t)(ev.noteOff.velocity * 127.0f);
                        midiEvents[numMidi]._pad = 0;
                        numMidi++;
                        break;
                    case 4: // kPolyPressureEvent
                        midiEvents[numMidi].sample_offset = ev.sampleOffset;
                        midiEvents[numMidi].status = 0xA0 | (ev.polyPressure.channel & 0x0F);
                        midiEvents[numMidi].data1 = ev.polyPressure.pitch & 0x7F;
                        midiEvents[numMidi].data2 = (uint8_t)(ev.polyPressure.pressure * 127.0f);
                        midiEvents[numMidi]._pad = 0;
                        numMidi++;
                        break;
                    case 6: // kNoteExpressionValueEvent
                        // Convert to CC-like event: status=0xF0 (custom),
                        // data1=typeId, data2=value*127
                        // typeId: 0=volume, 1=pan, 2=tuning, 3=vibrato, 4=expression, 5=brightness
                        midiEvents[numMidi].sample_offset = ev.sampleOffset;
                        midiEvents[numMidi].status = 0xF0; // marker for note expression
                        midiEvents[numMidi].data1 = (uint8_t)ev.noteExpressionValue.typeId;
                        midiEvents[numMidi].data2 = (uint8_t)(ev.noteExpressionValue.value * 127.0);
                        midiEvents[numMidi]._pad = (uint8_t)(ev.noteExpressionValue.noteId & 0xFF);
                        numMidi++;
                        break;
                }
            }
        }

        g_cb->process(ctx, inPtrs, outPtrs, numIn, numOut, numFrames,
                      midiEvents, numMidi,
                      transportPtr, paramChanges, numParamChanges);

        // Forward output events (MIDI output from instruments/effects)
        if (data->outputEvents && g_cb->get_output_event_count) {
            uint32_t outCount = g_cb->get_output_event_count(ctx);
            if (outCount > 0) {
                struct { void* vtbl; } *eventList = (decltype(eventList))data->outputEvents;
                struct OEVtbl {
                    tresult (*qi)(void*, const TUID, void**);
                    uint32 (*addRef)(void*);
                    uint32 (*release)(void*);
                    int32 (*getEventCount)(void*);
                    tresult (*getEvent)(void*, int32, void*);
                    tresult (*addEvent)(void*, void*);
                };
                auto* vtbl = (OEVtbl*)eventList->vtbl;

                for (uint32_t i = 0; i < outCount; i++) {
                    Vst3MidiEvent mev = {};
                    g_cb->get_output_event(ctx, i, &mev);
                    if (mev.status == 0) continue;

                    // Build VST3 Event
                    struct alignas(8) Vst3OutEvent {
                        int32 busIndex;
                        int32 sampleOffset;
                        double ppqPosition;
                        uint16_t flags;
                        uint16_t type;
                        char pad[4];
                        union {
                            struct { int16_t channel; int16_t pitch; float tuning; float velocity; int32 length; int32 noteId; } noteOn;
                            struct { int16_t channel; int16_t pitch; float velocity; int32 noteId; float tuning; } noteOff;
                        };
                    };
                    Vst3OutEvent ev = {};
                    ev.sampleOffset = mev.sample_offset;
                    uint8_t st = mev.status & 0xF0;
                    uint8_t ch = mev.status & 0x0F;
                    if (st == 0x90) {
                        ev.type = 0; // kNoteOnEvent
                        ev.noteOn.channel = ch;
                        ev.noteOn.pitch = mev.data1;
                        ev.noteOn.velocity = mev.data2 / 127.0f;
                        ev.noteOn.noteId = -1;
                        ev.noteOn.length = 0;
                        ev.noteOn.tuning = 0;
                    } else if (st == 0x80) {
                        ev.type = 1; // kNoteOffEvent
                        ev.noteOff.channel = ch;
                        ev.noteOff.pitch = mev.data1;
                        ev.noteOff.velocity = mev.data2 / 127.0f;
                        ev.noteOff.noteId = -1;
                    } else {
                        continue; // skip non-note events for now
                    }
                    vtbl->addEvent(data->outputEvents, &ev);
                }
            }
        }

        return kResultOk;
    }

    uint32 getTailSamples() {
        return (g_cb && ctx && g_cb->get_tail) ? g_cb->get_tail(ctx) : 0;
    }

    // --- IEditController ---

    tresult setComponentState(void* stream) {
        // In single-component mode, the component setState already loaded
        // the params. The controller just needs to acknowledge success so
        // the host knows the controller is in sync.
        (void)stream;
        return kResultOk;
    }
    tresult setECState(void*) { return kResultOk; }
    tresult getECState(void*) { return kResultOk; }

    int32 getParameterCount() {
        return g_num_params;
    }

    tresult getParameterInfo(int32 index, ParameterInfo* info) {
        if (!info || index < 0 || (uint32_t)index >= g_num_params) return kInvalidArgument;
        auto& p = g_params[index];
        info->id = p.id;
        str_to_char16(info->title, p.name, 128);
        str_to_char16(info->shortTitle, p.short_name, 128);
        str_to_char16(info->units, p.units, 128);
        info->stepCount = p.step_count;
        info->defaultNormalizedValue = p.default_normalized;
        info->unitId = ((uint32_t)index < 1024) ? g_param_unit_id[index] : 0;
        info->flags = p.flags;
        return kResultOk;
    }

    tresult getParamStringByValue(uint32 id, double valueNormalized, char16* string) {
        if (!string || !g_cb || !ctx) return kInvalidArgument;
        double plain = g_cb->param_denormalize(ctx, id, valueNormalized);
        char buf[128];
        uint32_t len = g_cb->param_format(ctx, id, plain, buf, sizeof(buf));
        if (len > 0) { str_to_char16(string, buf, 128); return kResultOk; }
        char tmp[32]; snprintf(tmp, sizeof(tmp), "%.2f", plain);
        str_to_char16(string, tmp, 128);
        return kResultOk;
    }

    tresult getParamValueByString(uint32, char16*, double*) { return kNotImplemented; }

    double normalizedParamToPlain(uint32 id, double normalized) {
        return (g_cb && ctx) ? g_cb->param_denormalize(ctx, id, normalized) : normalized;
    }

    double plainParamToNormalized(uint32 id, double plain) {
        return (g_cb && ctx) ? g_cb->param_normalize(ctx, id, plain) : plain;
    }

    double getParamNormalized(uint32 id) {
        if (!g_cb || !ctx) return 0;
        double plain = g_cb->param_get_value(ctx, id);
        return g_cb->param_normalize(ctx, id, plain);
    }

    tresult setParamNormalized(uint32 id, double value) {
        if (!g_cb || !ctx) return kResultFalse;
        if (inPerformEdit) return kResultOk; // skip host→plugin echo during GUI edit
        double plain = g_cb->param_denormalize(ctx, id, value);
        g_cb->param_set_value(ctx, id, plain);
        return kResultOk;
    }

    tresult setComponentHandler(void* handler) {
        if (componentHandler) {
            auto release_fn = (uint32 (*)(void*))(*(void***)componentHandler)[2];
            release_fn(componentHandler);
        }
        componentHandler = handler;
        if (componentHandler) {
            auto addRef_fn = (uint32 (*)(void*))(*(void***)componentHandler)[1];
            addRef_fn(componentHandler);
        }
        return kResultOk;
    }
    void* createView(FIDString /*name*/);
};

// ---------------------------------------------------------------------------
// IPlugView — minimal COM object for GUI embedding
// ---------------------------------------------------------------------------

struct IPlugViewVtbl {
    tresult (*queryInterface)(void*, const TUID, void**);
    uint32  (*addRef)(void*);
    uint32  (*release)(void*);
    tresult (*isPlatformTypeSupported)(void*, FIDString);
    tresult (*attached)(void*, void*, FIDString);
    tresult (*removed)(void*);
    tresult (*onWheel)(void*, float);
    tresult (*onKeyDown)(void*, char16, int16_t, int16_t);
    tresult (*onKeyUp)(void*, char16, int16_t, int16_t);
    tresult (*getSize)(void*, void*);  // ViewRect*
    tresult (*onSize)(void*, void*);
    tresult (*onFocus)(void*, int8);
    tresult (*setFrame)(void*, void*);
    tresult (*canResize)(void*);
    tresult (*checkSizeConstraint)(void*, void*);
};

struct ViewRect { int32 left; int32 top; int32 right; int32 bottom; };

struct TrucePlugView {
    IPlugViewVtbl* vtbl;
    int32_t refCount;
    void* ctx;  // Rust plugin context
};

static tresult pv_queryInterface(void* s, const TUID iid, void** obj) {
    if (iid_equal(iid, FUnknown_iid) || iid_equal(iid, IPlugView_iid)) {
        auto* pv = (TrucePlugView*)s;
        pv->refCount++;
        *obj = s;
        return kResultOk;
    }
    *obj = nullptr;
    return kResultFalse;
}
static uint32 pv_addRef(void* s) { return ++((TrucePlugView*)s)->refCount; }
static uint32 pv_release(void* s) {
    auto* pv = (TrucePlugView*)s;
    if (--pv->refCount <= 0) { free(pv); return 0; }
    return pv->refCount;
}
static tresult pv_isPlatformTypeSupported(void*, FIDString type) {
    #ifdef __APPLE__
    if (strcmp(type, kPlatformTypeNSView) == 0) return kResultOk;
    #endif
    return kResultFalse;
}
static tresult pv_attached(void* s, void* parent, FIDString /*type*/) {
    auto* pv = (TrucePlugView*)s;
    if (g_cb && pv->ctx) g_cb->gui_open(pv->ctx, parent);
    return kResultOk;
}
static tresult pv_removed(void* s) {
    auto* pv = (TrucePlugView*)s;
    if (g_cb && pv->ctx) g_cb->gui_close(pv->ctx);
    return kResultOk;
}
static tresult pv_getSize(void* s, void* rect) {
    auto* pv = (TrucePlugView*)s;
    auto* r = (ViewRect*)rect;
    if (g_cb && pv->ctx) {
        uint32_t w = 0, h = 0;
        g_cb->gui_get_size(pv->ctx, &w, &h);
        r->left = 0; r->top = 0;
        r->right = (int32)w; r->bottom = (int32)h;
        return kResultOk;
    }
    return kResultFalse;
}
static tresult pv_stub_false(void*) { return kResultFalse; }
static tresult pv_stub1(void*, float) { return kResultFalse; }
static tresult pv_stub2(void*, char16, int16_t, int16_t) { return kResultFalse; }
static tresult pv_stub3(void*, void*) { return kResultFalse; }
static tresult pv_stub4(void*, int8) { return kResultFalse; }

static IPlugViewVtbl g_plugview_vtbl = {
    pv_queryInterface, pv_addRef, pv_release,
    pv_isPlatformTypeSupported,
    pv_attached, pv_removed,
    pv_stub1,      // onWheel
    pv_stub2,      // onKeyDown
    pv_stub2,      // onKeyUp
    pv_getSize,
    pv_stub3,      // onSize
    pv_stub4,      // onFocus
    pv_stub3,      // setFrame
    pv_stub_false, // canResize
    pv_stub3,      // checkSizeConstraint
};

// ---------------------------------------------------------------------------
// Vtables — laid out exactly as C++ COM hosts expect
// ---------------------------------------------------------------------------

// We use a multi-vtable approach: the object has 3 vtable pointers at the start.
// queryInterface returns different offsets for different IIDs.
// This is the standard C++ multiple-inheritance COM layout.

struct IComponentVtbl {
    tresult (*queryInterface)(void*, const TUID, void**);
    uint32  (*addRef)(void*);
    uint32  (*release)(void*);
    tresult (*initialize)(void*, void*);
    tresult (*terminate)(void*);
    tresult (*getControllerClassId)(void*, TUID);
    tresult (*setIoMode)(void*, int32);
    int32   (*getBusCount)(void*, int32, int32);
    tresult (*getBusInfo)(void*, int32, int32, int32, BusInfo*);
    tresult (*getRoutingInfo)(void*, RoutingInfo*, RoutingInfo*);
    tresult (*activateBus)(void*, int32, int32, int32, TBool);
    tresult (*setActive)(void*, TBool);
    tresult (*setState)(void*, void*);
    tresult (*getState)(void*, void*);
};

struct IAudioProcessorVtbl {
    tresult (*queryInterface)(void*, const TUID, void**);
    uint32  (*addRef)(void*);
    uint32  (*release)(void*);
    tresult (*setBusArrangements)(void*, uint64_t*, int32, uint64_t*, int32);
    tresult (*getBusArrangement)(void*, int32, int32, uint64_t*);
    tresult (*canProcessSampleSize)(void*, int32);
    uint32  (*getLatencySamples)(void*);
    tresult (*setupProcessing)(void*, ProcessSetup*);
    tresult (*setProcessing)(void*, TBool);
    tresult (*process)(void*, ProcessData*);
    uint32  (*getTailSamples)(void*);
};

struct IEditControllerVtbl {
    tresult (*queryInterface)(void*, const TUID, void**);
    uint32  (*addRef)(void*);
    uint32  (*release)(void*);
    tresult (*initialize)(void*, void*);
    tresult (*terminate)(void*);
    tresult (*setComponentState)(void*, void*);
    tresult (*setState)(void*, void*);
    tresult (*getState)(void*, void*);
    int32   (*getParameterCount)(void*);
    tresult (*getParameterInfo)(void*, int32, ParameterInfo*);
    tresult (*getParamStringByValue)(void*, uint32, double, char16*);
    tresult (*getParamValueByString)(void*, uint32, char16*, double*);
    double  (*normalizedParamToPlain)(void*, uint32, double);
    double  (*plainParamToNormalized)(void*, uint32, double);
    double  (*getParamNormalized)(void*, uint32);
    tresult (*setParamNormalized)(void*, uint32, double);
    tresult (*setComponentHandler)(void*, void*);
    void*   (*createView)(void*, FIDString);
};

struct IEditControllerHostEditingVtbl {
    tresult (*queryInterface)(void*, const TUID, void**);
    uint32  (*addRef)(void*);
    uint32  (*release)(void*);
    tresult (*beginEditFromHost)(void*, uint32);
    tresult (*endEditFromHost)(void*, uint32);
};

struct IProcessContextRequirementsVtbl {
    tresult (*queryInterface)(void*, const TUID, void**);
    uint32  (*addRef)(void*);
    uint32  (*release)(void*);
    uint32  (*getProcessContextRequirements)(void*);
};

struct UnitInfo {
    int32 id;
    int32 parentUnitId;
    char16 name[128];
    int32 programListId; // -1 = no program list
};

struct IUnitInfoVtbl {
    tresult (*queryInterface)(void*, const TUID, void**);
    uint32  (*addRef)(void*);
    uint32  (*release)(void*);
    int32   (*getUnitCount)(void*);
    tresult (*getUnitInfo)(void*, int32, UnitInfo*);
    int32   (*getProgramListCount)(void*);
    tresult (*getProgramListInfo)(void*, int32, void*);
    tresult (*getProgramName)(void*, int32, int32, char16*);
    tresult (*getProgramInfo)(void*, int32, int32, const char*, char16*);
    tresult (*hasProgramPitchNames)(void*, int32, int32);
    tresult (*getProgramPitchName)(void*, int32, int32, int16_t, char16*);
    int32   (*getSelectedUnit)(void*);
    tresult (*selectUnit)(void*, int32);
    tresult (*getUnitByBus)(void*, int32, int32, int32, int32, int32*);
    tresult (*setUnitProgramData)(void*, int32, int32, void*);
};

// The actual COM object layout: 4 vtable pointers followed by the C++ object
struct TruceComponentCOM {
    IComponentVtbl* vtbl_component;
    IAudioProcessorVtbl* vtbl_processor;
    IEditControllerVtbl* vtbl_controller;
    IEditControllerHostEditingVtbl* vtbl_host_editing;
    IProcessContextRequirementsVtbl* vtbl_pcr;
    IUnitInfoVtbl* vtbl_unitinfo;
    TruceComponent impl;
};

// Deferred: createView
void* TruceComponent::createView(FIDString /*name*/) {
    if (!g_cb || !ctx) return nullptr;
    if (!g_cb->gui_has_editor(ctx)) return nullptr;
    auto* pv = (TrucePlugView*)calloc(1, sizeof(TrucePlugView));
    pv->vtbl = &g_plugview_vtbl;
    pv->refCount = 1;
    pv->ctx = ctx;
    return pv;
}

// Deferred definition of queryInterface (needs complete TruceComponentCOM)
tresult TruceComponent::queryInterface(void* comBase, const TUID iid, void** obj) {
    auto* com = reinterpret_cast<TruceComponentCOM*>(comBase);
    if (iid_equal(iid, FUnknown_iid) ||
        iid_equal(iid, IPluginBase_iid) ||
        iid_equal(iid, IComponent_iid)) {
        addRef();
        *obj = &com->vtbl_component;
        return kResultOk;
    }
    if (iid_equal(iid, IAudioProcessor_iid)) {
        addRef();
        *obj = &com->vtbl_processor;
        return kResultOk;
    }
    if (iid_equal(iid, IEditController_iid)) {
        addRef();
        *obj = &com->vtbl_controller;
        return kResultOk;
    }
    if (iid_equal(iid, IEditControllerHostEditing_iid)) {
        addRef();
        *obj = &com->vtbl_host_editing;
        return kResultOk;
    }
    if (iid_equal(iid, IProcessContextRequirements_iid)) {
        addRef();
        *obj = &com->vtbl_pcr;
        return kResultOk;
    }
    if (iid_equal(iid, IUnitInfo_iid) && g_num_units > 1) {
        addRef();
        *obj = &com->vtbl_unitinfo;
        return kResultOk;
    }
    *obj = nullptr;
    return kResultFalse;
}

// Helper to get TruceComponentCOM from any vtable pointer
static TruceComponentCOM* com_from_component(void* self) {
    return reinterpret_cast<TruceComponentCOM*>(self);
}
static TruceComponentCOM* com_from_processor(void* self) {
    return reinterpret_cast<TruceComponentCOM*>(
        reinterpret_cast<char*>(self) - sizeof(void*));
}
static TruceComponentCOM* com_from_controller(void* self) {
    return reinterpret_cast<TruceComponentCOM*>(
        reinterpret_cast<char*>(self) - 2 * sizeof(void*));
}
static TruceComponentCOM* com_from_host_editing(void* self) {
    return reinterpret_cast<TruceComponentCOM*>(
        reinterpret_cast<char*>(self) - 3 * sizeof(void*));
}
static TruceComponentCOM* com_from_pcr(void* self) {
    return reinterpret_cast<TruceComponentCOM*>(
        reinterpret_cast<char*>(self) - 4 * sizeof(void*));
}
static TruceComponentCOM* com_from_unitinfo(void* self) {
    return reinterpret_cast<TruceComponentCOM*>(
        reinterpret_cast<char*>(self) - 5 * sizeof(void*));
}

// --- Component vtable functions ---
#define COMP(self) (com_from_component(self)->impl)
static tresult comp_qi(void* s, const TUID iid, void** obj) { return COMP(s).queryInterface(com_from_component(s), iid, obj); }
static uint32 comp_addRef(void* s) { return COMP(s).addRef(); }
static uint32 comp_release(void* s) { auto* com = com_from_component(s); auto r = com->impl.release(); if (r == 0) { com->impl.~TruceComponent(); free(com); } return r; }
static tresult comp_init(void* s, void* ctx) { return COMP(s).initialize(ctx); }
static tresult comp_term(void* s) { return COMP(s).terminate(); }
static tresult comp_getCtrlId(void* s, TUID id) { return COMP(s).getControllerClassId(id); }
static tresult comp_setIoMode(void* s, int32 m) { return COMP(s).setIoMode(m); }
static int32 comp_getBusCount(void* s, int32 t, int32 d) { return COMP(s).getBusCount(t, d); }
static tresult comp_getBusInfo(void* s, int32 t, int32 d, int32 i, BusInfo* b) { return COMP(s).getBusInfo(t, d, i, b); }
static tresult comp_getRouting(void* s, RoutingInfo* a, RoutingInfo* b) { return COMP(s).getRoutingInfo(a, b); }
static tresult comp_activateBus(void* s, int32 t, int32 d, int32 i, TBool st) { return COMP(s).activateBus(t, d, i, st); }
static tresult comp_setActive(void* s, TBool st) { return COMP(s).setActive(st); }
static tresult comp_setState(void* s, void* st) { return COMP(s).setState(st); }
static tresult comp_getState(void* s, void* st) { return COMP(s).getState(st); }

// --- Processor vtable functions ---
#define PROC(self) (com_from_processor(self)->impl)
static tresult proc_qi(void* s, const TUID iid, void** obj) { return PROC(s).queryInterface(com_from_processor(s), iid, obj); }
static uint32 proc_addRef(void* s) { return PROC(s).addRef(); }
static uint32 proc_release(void* s) { auto* com = com_from_processor(s); auto r = com->impl.release(); if (r == 0) { com->impl.~TruceComponent(); free(com); } return r; }
static tresult proc_setBusArr(void* s, uint64_t* a, int32 b, uint64_t* c, int32 d) { return PROC(s).setBusArrangements(a, b, c, d); }
static tresult proc_getBusArr(void* s, int32 d, int32 i, uint64_t* a) { return PROC(s).getBusArrangement(d, i, a); }
static tresult proc_canProcess(void* s, int32 ss) { return PROC(s).canProcessSampleSize(ss); }
static uint32 proc_getLatency(void* s) { return PROC(s).getLatencySamples(); }
static tresult proc_setup(void* s, ProcessSetup* p) { return PROC(s).setupProcessing(p); }
static tresult proc_setProc(void* s, TBool st) { return PROC(s).setProcessing(st); }
static tresult proc_process(void* s, ProcessData* d) { return PROC(s).process(d); }
static uint32 proc_getTail(void* s) { return PROC(s).getTailSamples(); }

// --- Controller vtable functions ---
#define CTRL(self) (com_from_controller(self)->impl)
static tresult ctrl_qi(void* s, const TUID iid, void** obj) { return CTRL(s).queryInterface(com_from_controller(s), iid, obj); }
static uint32 ctrl_addRef(void* s) { return CTRL(s).addRef(); }
static uint32 ctrl_release(void* s) { auto* com = com_from_controller(s); auto r = com->impl.release(); if (r == 0) { com->impl.~TruceComponent(); free(com); } return r; }
static tresult ctrl_init(void* s, void* ctx) { return CTRL(s).initialize(ctx); }
static tresult ctrl_term(void* s) { return CTRL(s).terminate(); }
static tresult ctrl_setCompState(void* s, void* st) { return CTRL(s).setComponentState(st); }
static tresult ctrl_setState(void* s, void* st) { return CTRL(s).setECState(st); }
static tresult ctrl_getState(void* s, void* st) { return CTRL(s).getECState(st); }
static int32 ctrl_getParamCount(void* s) { return CTRL(s).getParameterCount(); }
static tresult ctrl_getParamInfo(void* s, int32 i, ParameterInfo* p) { return CTRL(s).getParameterInfo(i, p); }
static tresult ctrl_getParamStr(void* s, uint32 id, double v, char16* str) { return CTRL(s).getParamStringByValue(id, v, str); }
static tresult ctrl_getParamVal(void* s, uint32 id, char16* str, double* v) { return CTRL(s).getParamValueByString(id, str, v); }
static double ctrl_n2p(void* s, uint32 id, double v) { return CTRL(s).normalizedParamToPlain(id, v); }
static double ctrl_p2n(void* s, uint32 id, double v) { return CTRL(s).plainParamToNormalized(id, v); }
static double ctrl_getPN(void* s, uint32 id) { return CTRL(s).getParamNormalized(id); }
static tresult ctrl_setPN(void* s, uint32 id, double v) { return CTRL(s).setParamNormalized(id, v); }
static tresult ctrl_setHandler(void* s, void* h) { return CTRL(s).setComponentHandler(h); }
static void* ctrl_createView(void* s, FIDString n) { return CTRL(s).createView(n); }

// --- HostEditing vtable functions ---
#define HEDIT(self) (com_from_host_editing(self)->impl)
static tresult hedit_qi(void* s, const TUID iid, void** obj) { return HEDIT(s).queryInterface(com_from_host_editing(s), iid, obj); }
static uint32 hedit_addRef(void* s) { return HEDIT(s).addRef(); }
static uint32 hedit_release(void* s) { auto* com = com_from_host_editing(s); auto r = com->impl.release(); if (r == 0) { com->impl.~TruceComponent(); free(com); } return r; }
static tresult hedit_beginEditFromHost(void*, uint32) { return kResultOk; }
static tresult hedit_endEditFromHost(void*, uint32) { return kResultOk; }

// Static vtables
static IComponentVtbl g_comp_vtbl = {
    comp_qi, comp_addRef, comp_release, comp_init, comp_term,
    comp_getCtrlId, comp_setIoMode, comp_getBusCount, comp_getBusInfo,
    comp_getRouting, comp_activateBus, comp_setActive, comp_setState, comp_getState
};

static IAudioProcessorVtbl g_proc_vtbl = {
    proc_qi, proc_addRef, proc_release,
    proc_setBusArr, proc_getBusArr, proc_canProcess, proc_getLatency,
    proc_setup, proc_setProc, proc_process, proc_getTail
};

static IEditControllerVtbl g_ctrl_vtbl = {
    ctrl_qi, ctrl_addRef, ctrl_release, ctrl_init, ctrl_term,
    ctrl_setCompState, ctrl_setState, ctrl_getState,
    ctrl_getParamCount, ctrl_getParamInfo, ctrl_getParamStr, ctrl_getParamVal,
    ctrl_n2p, ctrl_p2n, ctrl_getPN, ctrl_setPN, ctrl_setHandler, ctrl_createView
};

static IEditControllerHostEditingVtbl g_hedit_vtbl = {
    hedit_qi, hedit_addRef, hedit_release,
    hedit_beginEditFromHost, hedit_endEditFromHost
};

// --- ProcessContextRequirements vtable functions ---
#define PCR(self) (com_from_pcr(self)->impl)
static tresult pcr_qi(void* s, const TUID iid, void** obj) { return PCR(s).queryInterface(com_from_pcr(s), iid, obj); }
static uint32 pcr_addRef(void* s) { return PCR(s).addRef(); }
static uint32 pcr_release(void* s) { auto* com = com_from_pcr(s); auto r = com->impl.release(); if (r == 0) { com->impl.~TruceComponent(); free(com); } return r; }
static uint32 pcr_getReqs(void*) {
    // Request all context fields
    return (1 << 1) | (1 << 2) | (1 << 3) | (1 << 4) | (1 << 5) |
           (1 << 6) | (1 << 7) | (1 << 8) | (1 << 9) | (1 << 10);
}

static IProcessContextRequirementsVtbl g_pcr_vtbl = {
    pcr_qi, pcr_addRef, pcr_release, pcr_getReqs
};

// --- UnitInfo vtable functions ---
#define UINFO(self) (com_from_unitinfo(self)->impl)
static tresult ui_qi(void* s, const TUID iid, void** obj) { return UINFO(s).queryInterface(com_from_unitinfo(s), iid, obj); }
static uint32 ui_addRef(void* s) { return UINFO(s).addRef(); }
static uint32 ui_release(void* s) { auto* com = com_from_unitinfo(s); auto r = com->impl.release(); if (r == 0) { com->impl.~TruceComponent(); free(com); } return r; }
static int32 ui_getUnitCount(void*) { return g_num_units; }
static tresult ui_getUnitInfo(void*, int32 idx, UnitInfo* out) {
    if (idx < 0 || idx >= g_num_units || !out) return kInvalidArgument;
    out->id = g_units[idx].id;
    out->parentUnitId = g_units[idx].parentId;
    str_to_char16(out->name, g_units[idx].name, 128);
    out->programListId = -1; // kNoProgramListId
    return kResultOk;
}
static int32 ui_getProgramListCount(void*) { return 0; }
static tresult ui_stub_not_impl() { return kNotImplemented; }

static IUnitInfoVtbl g_unitinfo_vtbl = {
    ui_qi, ui_addRef, ui_release,
    ui_getUnitCount, ui_getUnitInfo, ui_getProgramListCount,
    (tresult(*)(void*,int32,void*))ui_stub_not_impl,
    (tresult(*)(void*,int32,int32,char16*))ui_stub_not_impl,
    (tresult(*)(void*,int32,int32,const char*,char16*))ui_stub_not_impl,
    (tresult(*)(void*,int32,int32))ui_stub_not_impl,
    (tresult(*)(void*,int32,int32,int16_t,char16*))ui_stub_not_impl,
    [](void*) -> int32 { return 0; },
    [](void*, int32) -> tresult { return kResultOk; },
    (tresult(*)(void*,int32,int32,int32,int32,int32*))ui_stub_not_impl,
    (tresult(*)(void*,int32,int32,void*))ui_stub_not_impl,
};

static TruceComponentCOM* create_component() {
    auto* com = (TruceComponentCOM*)calloc(1, sizeof(TruceComponentCOM));
    if (!com) return nullptr;
    com->vtbl_component = &g_comp_vtbl;
    com->vtbl_processor = &g_proc_vtbl;
    com->vtbl_controller = &g_ctrl_vtbl;
    com->vtbl_host_editing = &g_hedit_vtbl;
    com->vtbl_pcr = &g_pcr_vtbl;
    com->vtbl_unitinfo = &g_unitinfo_vtbl;
    new (&com->impl) TruceComponent();
    return com;
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

struct FactoryCOM {
    void* vtbl;
    std::atomic<int32> refCount{1};
};

// IPluginFactory2 and IPluginFactory3 IIDs
static const TUID IPluginFactory2_iid = {I(0x00),I(0x07),I(0xB6),I(0x50),I(0xF2),I(0x4B),I(0x4C),I(0x0B),I(0xA4),I(0x64),I(0xED),I(0xB9),I(0xF0),I(0x0B),I(0x2A),I(0xBB)};
static const TUID IPluginFactory3_iid = {I(0x45),I(0x55),I(0xA2),I(0xAB),I(0xC1),I(0x23),I(0x4E),I(0x57),I(0x9B),I(0x12),I(0x29),I(0x10),I(0x36),I(0x87),I(0x89),I(0x31)};

static tresult factory_qi(void* self, const TUID iid, void** obj) {
    if (iid_equal(iid, FUnknown_iid) || iid_equal(iid, IPluginFactory_iid) ||
        iid_equal(iid, IPluginFactory2_iid) || iid_equal(iid, IPluginFactory3_iid)) {
        auto* f = (FactoryCOM*)self;
        f->refCount++;
        *obj = self;
        return kResultOk;
    }
    *obj = nullptr;
    return kResultFalse;
}

static uint32 factory_addRef(void* self) {
    return ++(((FactoryCOM*)self)->refCount);
}

static uint32 factory_release(void* self) {
    auto r = --(((FactoryCOM*)self)->refCount);
    // Don't free — factory is a global static
    return r;
}

static tresult factory_getInfo(void*, PFactoryInfo* info) {
    if (!info || !g_desc) return kInvalidArgument;
    memset(info, 0, sizeof(*info));
    str_to_char8(info->vendor, g_desc->vendor, 64);
    str_to_char8(info->url, g_desc->url, 256);
    str_to_char8(info->email, g_desc->email ? g_desc->email : "", 128);
    info->flags = (1 << 4); // kUnicode
    return kResultOk;
}

static int32 factory_countClasses(void*) { return 1; }

static tresult factory_getClassInfo(void*, int32 index, PClassInfo* info) {
    if (index != 0 || !info || !g_desc) return kInvalidArgument;
    memcpy(info->cid, g_desc->cid, 16);
    info->cardinality = 0x7FFFFFFF; // kManyInstances
    str_to_char8(info->category, g_desc->category, 32);
    str_to_char8(info->name, g_desc->name, 64);
    return kResultOk;
}

// IPluginFactory2::getClassInfo2
struct PClassInfo2 {
    TUID cid; int32 cardinality; char8 category[32]; char8 name[64];
    uint32 classFlags; char8 subCategories[128]; char8 vendor[64]; char8 version[64]; char8 sdkVersion[64];
};

static tresult factory_getClassInfo2(void*, int32 index, PClassInfo2* info) {
    if (index != 0 || !info || !g_desc) return kInvalidArgument;
    memset(info, 0, sizeof(*info));
    memcpy(info->cid, g_desc->cid, 16);
    info->cardinality = 0x7FFFFFFF;
    str_to_char8(info->category, g_desc->category, 32);
    str_to_char8(info->name, g_desc->name, 64);
    info->classFlags = 0; // single-component (processor + controller in one object)
    str_to_char8(info->subCategories, g_desc->subcategories, 128);
    str_to_char8(info->vendor, g_desc->vendor, 64);
    str_to_char8(info->version, g_desc->version, 64);
    str_to_char8(info->sdkVersion, "VST 3.7.1", 64);
    return kResultOk;
}

// IPluginFactory3::getClassInfoUnicode
struct PClassInfoW {
    TUID cid; int32 cardinality; char8 category[32]; char16 name[64];
    uint32 classFlags; char8 subCategories[128]; char16 vendor[64]; char16 version[64]; char16 sdkVersion[64];
};

static tresult factory_getClassInfoW(void*, int32 index, PClassInfoW* info) {
    if (index != 0 || !info || !g_desc) return kInvalidArgument;
    memset(info, 0, sizeof(*info));
    memcpy(info->cid, g_desc->cid, 16);
    info->cardinality = 0x7FFFFFFF;
    str_to_char8(info->category, g_desc->category, 32);
    str_to_char16(info->name, g_desc->name, 64);
    info->classFlags = 0; // single-component
    str_to_char8(info->subCategories, g_desc->subcategories, 128);
    str_to_char16(info->vendor, g_desc->vendor, 64);
    str_to_char16(info->version, g_desc->version, 64);
    str_to_char16(info->sdkVersion, "VST 3.7.1", 64);
    return kResultOk;
}

static tresult factory_setHostContext(void*, void*) { return kResultOk; }

static tresult factory_createInstance(void*, FIDString cid, FIDString iid, void** obj) {
    if (!cid || !iid || !obj || !g_desc) return kInvalidArgument;
    if (memcmp(cid, g_desc->cid, 16) != 0) return kResultFalse;

    auto* com = create_component();
    if (!com) return kResultFalse;

    tresult r = com->impl.queryInterface(com, reinterpret_cast<const int8_t*>(iid), obj);
    if (r != kResultOk) {
        com->impl.~TruceComponent();
        free(com);
    } else {
        // QI added a ref, release the initial one
        com->impl.release();
    }
    return r;
}

// Combined vtable: IPluginFactory + IPluginFactory2 + IPluginFactory3
// Laid out as C++ single-inheritance: base methods first, derived appended
struct IPluginFactoryVtbl {
    // FUnknown
    tresult (*queryInterface)(void*, const TUID, void**);
    uint32  (*addRef)(void*);
    uint32  (*release)(void*);
    // IPluginFactory
    tresult (*getFactoryInfo)(void*, PFactoryInfo*);
    int32   (*countClasses)(void*);
    tresult (*getClassInfo)(void*, int32, PClassInfo*);
    tresult (*createInstance)(void*, FIDString, FIDString, void**);
    // IPluginFactory2
    tresult (*getClassInfo2)(void*, int32, PClassInfo2*);
    // IPluginFactory3
    tresult (*getClassInfoUnicode)(void*, int32, PClassInfoW*);
    tresult (*setHostContext)(void*, void*);
};

static IPluginFactoryVtbl g_factory_vtbl = {
    factory_qi, factory_addRef, factory_release,
    factory_getInfo, factory_countClasses, factory_getClassInfo, factory_createInstance,
    factory_getClassInfo2,
    factory_getClassInfoW, factory_setHostContext
};

static FactoryCOM g_factory = { &g_factory_vtbl, {1} };

// ---------------------------------------------------------------------------
// Exported entry points
// ---------------------------------------------------------------------------

extern "C" {

void truce_vst3_register(
    const Vst3PluginDescriptor* descriptor,
    const Vst3Callbacks* callbacks,
    const Vst3ParamDescriptor* params,
    uint32_t num_params
) {
    g_desc = descriptor;
    g_cb = callbacks;
    g_params = params;
    g_num_params = num_params;
    build_unit_map();
}

void* truce_vst3_get_factory() {
    g_factory.refCount = 1;
    return &g_factory;
}

// --- IComponentHandler host-notification callbacks ---

static TruceComponent* ctx_lookup(void* ctx) {
    for (int i = 0; i < kMaxInstances; i++) {
        if (g_ctx_map_key[i] == ctx) return g_ctx_map_comp[i];
    }
    return nullptr;
}

void truce_vst3_begin_edit(void* ctx, uint32_t id) {
    auto* comp = ctx_lookup(ctx);
    if (!comp || !comp->componentHandler) return;
    auto beginEdit = (tresult (*)(void*, uint32))(*(void***)comp->componentHandler)[3];
    beginEdit(comp->componentHandler, id);
}

void truce_vst3_perform_edit(void* ctx, uint32_t id, double normalized) {
    auto* comp = ctx_lookup(ctx);
    if (!comp || !comp->componentHandler) return;
    comp->inPerformEdit = true;
    auto performEdit = (tresult (*)(void*, uint32, double))(*(void***)comp->componentHandler)[4];
    performEdit(comp->componentHandler, id, normalized);
    comp->inPerformEdit = false;
}

void truce_vst3_end_edit(void* ctx, uint32_t id) {
    auto* comp = ctx_lookup(ctx);
    if (!comp || !comp->componentHandler) return;
    auto endEdit = (tresult (*)(void*, uint32))(*(void***)comp->componentHandler)[5];
    endEdit(comp->componentHandler, id);
}

} // extern "C"
