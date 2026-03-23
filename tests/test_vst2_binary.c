/**
 * VST2 binary integration test.
 *
 * Loads the compiled VST2 dylib via dlopen, calls VSTPluginMain,
 * and verifies the AEffect struct fields.
 *
 * Build: cc -o test_vst2 tests/test_vst2_binary.c -ldl
 * Run:   ./test_vst2 target/release/libtruce_example_gain.dylib
 *        ./test_vst2 target/release/libtruce_example_synth.dylib
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <dlfcn.h>
#include <stdint.h>

/* Include the actual VST2 types from the shim */
#include "../crates/truce-vst2/shim/vst2_types.h"

static int tests_run = 0;
static int tests_passed = 0;

#define CHECK(cond, msg) do { \
    tests_run++; \
    if (cond) { tests_passed++; printf("  PASS: %s\n", msg); } \
    else { printf("  FAIL: %s\n", msg); } \
} while(0)

static VstIntPtr host_callback(AEffect* effect, int32_t opcode, int32_t index,
                               VstIntPtr value, void* ptr, float opt) {
    (void)effect; (void)index; (void)value; (void)ptr; (void)opt;
    if (opcode == audioMasterVersion) return 2400;
    return 0;
}

int main(int argc, char** argv) {
    if (argc < 2) {
        fprintf(stderr, "Usage: %s <path-to-vst2-dylib> [--synth]\n", argv[0]);
        return 1;
    }

    const char* path = argv[1];
    int is_synth = (argc > 2 && strcmp(argv[2], "--synth") == 0);

    printf("Testing VST2: %s %s\n", path, is_synth ? "(instrument)" : "(effect)");

    /* Load */
    void* lib = dlopen(path, RTLD_NOW);
    if (!lib) {
        printf("  FAIL: dlopen: %s\n", dlerror());
        return 1;
    }

    typedef AEffect* (*VSTPluginMainFn)(audioMasterCallback);
    VSTPluginMainFn pluginMain = (VSTPluginMainFn)dlsym(lib, "VSTPluginMain");
    if (!pluginMain) pluginMain = (VSTPluginMainFn)dlsym(lib, "main");
    CHECK(pluginMain != NULL, "VSTPluginMain symbol found");
    if (!pluginMain) { dlclose(lib); return 1; }

    /* Create instance */
    AEffect* effect = pluginMain(host_callback);
    CHECK(effect != NULL, "VSTPluginMain returns non-null");
    if (!effect) { dlclose(lib); return 1; }

    /* Magic */
    CHECK(effect->magic == kVstMagic, "magic == 'VstP'");

    /* Params */
    CHECK(effect->numParams > 0, "numParams > 0");
    printf("    numParams=%d\n", effect->numParams);

    /* I/O */
    if (is_synth) {
        CHECK(effect->numInputs == 0, "instrument: numInputs == 0");
        CHECK(effect->numOutputs == 2, "instrument: numOutputs == 2");
        CHECK(effect->flags & effFlagsIsSynth, "instrument: effFlagsIsSynth set");
    } else {
        CHECK(effect->numInputs == 2, "effect: numInputs == 2");
        CHECK(effect->numOutputs == 2, "effect: numOutputs == 2");
        CHECK(!(effect->flags & effFlagsIsSynth), "effect: effFlagsIsSynth not set");
    }

    /* Flags */
    CHECK(effect->flags & effFlagsCanReplacing, "effFlagsCanReplacing set");
    CHECK(effect->flags & effFlagsProgramChunks, "effFlagsProgramChunks set");

    /* processReplacing */
    CHECK(effect->processReplacing != NULL, "processReplacing not null");

    /* UniqueID */
    CHECK(effect->uniqueID != 0, "uniqueID is non-zero");
    printf("    uniqueID=0x%08x ('%c%c%c%c')\n", effect->uniqueID,
           (effect->uniqueID >> 24) & 0xFF, (effect->uniqueID >> 16) & 0xFF,
           (effect->uniqueID >> 8) & 0xFF, effect->uniqueID & 0xFF);

    /* Dispatcher: open */
    effect->dispatcher(effect, effOpen, 0, 0, NULL, 0);

    /* Set sample rate + block size + resume */
    effect->dispatcher(effect, effSetSampleRate, 0, 0, NULL, 44100.0f);
    effect->dispatcher(effect, effSetBlockSize, 0, 512, NULL, 0);
    effect->dispatcher(effect, effMainsChanged, 0, 1, NULL, 0);

    /* Parameter get/set round-trip */
    if (effect->numParams > 0) {
        float orig = effect->getParameter(effect, 0);
        effect->setParameter(effect, 0, 0.75f);
        float after = effect->getParameter(effect, 0);
        CHECK(after != orig || orig == 0.75f, "setParameter/getParameter round-trip");
        printf("    param[0]: before=%.3f, set=0.75, after=%.3f\n", orig, after);
    }

    /* Product string */
    char product[64] = {};
    effect->dispatcher(effect, effGetProductString, 0, 0, product, 0);
    CHECK(strlen(product) > 0, "effGetProductString returns non-empty");
    printf("    product: %s\n", product);

    /* Vendor string */
    char vendor[64] = {};
    effect->dispatcher(effect, effGetVendorString, 0, 0, vendor, 0);
    CHECK(strlen(vendor) > 0, "effGetVendorString returns non-empty");
    printf("    vendor: %s\n", vendor);

    /* Process audio */
    float inL[512] = {}, inR[512] = {};
    float outL[512] = {}, outR[512] = {};
    float* ins[2] = { inL, inR };
    float* outs[2] = { outL, outR };

    /* Fill input with non-zero for effects */
    if (!is_synth) {
        for (int i = 0; i < 512; i++) { inL[i] = 0.5f; inR[i] = 0.5f; }
    }

    effect->processReplacing(effect, ins, outs, 512);

    if (!is_synth) {
        /* Effect should pass audio through (at default gain) */
        int has_output = 0;
        for (int i = 0; i < 512; i++) {
            if (outL[i] != 0.0f || outR[i] != 0.0f) { has_output = 1; break; }
        }
        CHECK(has_output, "processReplacing produces output for effect");
    }

    /* State save/load */
    void* chunk = NULL;
    int32_t chunkSize = (int32_t)effect->dispatcher(effect, effGetChunk, 0, 0, &chunk, 0);
    CHECK(chunkSize > 0, "effGetChunk returns data");
    printf("    chunk size: %d bytes\n", chunkSize);

    if (chunkSize > 0 && chunk) {
        /* Modify a param, reload state, verify it restores */
        if (effect->numParams > 0) {
            float before = effect->getParameter(effect, 0);
            effect->setParameter(effect, 0, 0.0f);
            effect->dispatcher(effect, effSetChunk, 0, chunkSize, chunk, 0);
            float restored = effect->getParameter(effect, 0);
            CHECK(restored == before || (restored > before - 0.01f && restored < before + 0.01f),
                  "effSetChunk restores parameter values");
        }
    }

    /* Suspend + close */
    effect->dispatcher(effect, effMainsChanged, 0, 0, NULL, 0);
    effect->dispatcher(effect, effClose, 0, 0, NULL, 0);

    dlclose(lib);

    printf("\nResults: %d/%d passed\n", tests_passed, tests_run);
    return (tests_passed == tests_run) ? 0 : 1;
}
