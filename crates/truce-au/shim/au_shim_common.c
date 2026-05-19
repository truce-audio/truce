/**
 * Shared globals and registration for AU v2 and v3 shims.
 *
 * The same compiled framework dylib serves both v2 (.component) and
 * v3 (.appex) - the appex compiles its Swift AUAudioUnit subclass
 * separately and reads g_callbacks/g_descriptor/etc. out of this
 * dylib at runtime via dynamic symbol lookup.
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

// Constructor: populates globals when the dylib is loaded.
__attribute__((constructor))
static void au_shim_constructor(void) {
    truce_au_init();
}

// Weak stubs for symbols the `export_au!` macro emits in the *consumer*
// cdylib. They let `truce-au` build its own (unused) cdylib target -
// which is the only way cargo stops warning about
// `rustc-link-arg-cdylib` from this rlib. Consumer-supplied strong
// definitions override these at link time in the real plugin dylib.
__attribute__((weak))
void truce_au_init(void) {}
__attribute__((weak))
void *TruceAUFactory(const void *desc) {
    (void)desc;
    return NULL;
}
