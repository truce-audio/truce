/**
 * Shared globals and registration for AU v2 and v3 shims.
 * Always compiled regardless of TRUCE_AU_VERSION.
 */

#include "au_shim_types.h"
#include <stdio.h>

// Global state shared between v2 and v3 shims.
// visibility("default") ensures these are exported from the framework dylib
// so the appex binary (which compiles the ObjC classes separately) can access them.
__attribute__((visibility("default"))) const AuPluginDescriptor *g_descriptor = NULL;
__attribute__((visibility("default"))) const AuCallbacks *g_callbacks = NULL;
__attribute__((visibility("default"))) const AuParamDescriptor *g_param_descriptors = NULL;
__attribute__((visibility("default"))) uint32_t g_num_params = 0;

static int g_registered = 0;

void truce_au_register(
    const AuPluginDescriptor *descriptor,
    const AuCallbacks *callbacks,
    const AuParamDescriptor *param_descriptors,
    uint32_t num_params
) {
    if (g_registered) return;

    g_descriptor = descriptor;
    g_callbacks = callbacks;
    g_param_descriptors = param_descriptors;
    g_num_params = num_params;
    g_registered = 1;
}

// Defined in au_shim.m — registers the AUAudioUnit subclass for the v3→v2 bridge.
// Weak: no-op if v3 shim is not compiled (v2-only builds).
__attribute__((weak))
void truce_au_v3_register_subclass(void) {}

// Constructor: populates globals when the dylib is loaded.
__attribute__((constructor))
static void au_shim_constructor(void) {
    truce_au_init();
}

// v2 factory bridge — defined here so it always exists for the Rust linker.
// The actual implementation is in au_v2_shim.c (only compiled for v2 builds).
// For v3-only builds, this weak symbol returns NULL.
__attribute__((weak))
void *truce_au_v2_factory_bridge(const void *desc) {
    (void)desc;
    return NULL; // v2 shim not compiled — overridden when au_v2_shim.c is linked
}
