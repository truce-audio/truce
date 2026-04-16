/**
 * AAX plugin entry point — GetEffectDescriptions.
 *
 * Loads the Rust cdylib via the bridge, reads the plugin descriptor,
 * and registers with Pro Tools.
 */

#include "TruceAAX_Parameters.h"
#include "TruceAAX_GUI.h"
#include "TruceAAX_Bridge.h"

#include "AAX_ICollection.h"
#include "AAX_IComponentDescriptor.h"
#include "AAX_IEffectDescriptor.h"
#include "AAX_IPropertyMap.h"

#include <cstdio>

#ifdef __APPLE__
#include <CoreFoundation/CoreFoundation.h>
#include <dlfcn.h>
#endif

#ifdef _WIN32
#include <windows.h>
#endif

// Get the .aaxplugin bundle path at runtime.
//
// macOS layout:
//   Plugin.aaxplugin/Contents/MacOS/Plugin
//   → walk up 3 levels to reach Plugin.aaxplugin
//
// Windows layout:
//   Plugin.aaxplugin/Contents/x64/Plugin.aaxplugin
//   → walk up 2 levels to reach Plugin.aaxplugin
static bool GetBundlePath(char* out, size_t outLen) {
#ifdef __APPLE__
    Dl_info info;
    if (dladdr((void*)&GetBundlePath, &info)) {
        char path[2048];
        strncpy(path, info.dli_fname, sizeof(path));
        // Go up: Plugin → MacOS → Contents → Plugin.aaxplugin
        for (int i = 0; i < 3; i++) {
            char* last = strrchr(path, '/');
            if (last) *last = 0;
        }
        strncpy(out, path, outLen);
        return true;
    }
    return false;
#elif defined(_WIN32)
    HMODULE hm = NULL;
    if (!GetModuleHandleExA(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS
                | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            (LPCSTR)&GetBundlePath, &hm)) {
        return false;
    }
    char path[2048] = {};
    DWORD n = GetModuleFileNameA(hm, path, sizeof(path));
    if (n == 0 || n >= sizeof(path)) return false;
    // path is e.g. C:\...\Plugin.aaxplugin\Contents\x64\Plugin.aaxplugin
    // Walk up: binary → x64 → Contents → Plugin.aaxplugin
    for (int i = 0; i < 3; i++) {
        char* last = strrchr(path, '\\');
        if (!last) return false;
        *last = 0;
    }
    strncpy(out, path, outLen);
    if (outLen > 0) out[outLen - 1] = 0;
    return true;
#else
    (void)out; (void)outLen;
    return false;
#endif
}

// AAX entry point — called by Pro Tools on plugin load
AAX_Result GetEffectDescriptions(AAX_ICollection* outCollection) {
    // Load the Rust cdylib
    if (!g_bridge_loaded) {
        char bundlePath[2048] = {};
        if (!GetBundlePath(bundlePath, sizeof(bundlePath))) {
            fprintf(stderr, "[truce-aax] Could not determine bundle path\n");
            return AAX_ERROR_NULL_OBJECT;
        }
        if (!TruceBridge_Load(&g_bridge, bundlePath)) {
            fprintf(stderr, "[truce-aax] Failed to load Rust plugin\n");
            return AAX_ERROR_NULL_OBJECT;
        }
    }

    // Create effect descriptor
    AAX_IEffectDescriptor* desc = outCollection->NewDescriptor();
    if (!desc) return AAX_ERROR_NULL_OBJECT;

    // Names (Pro Tools uses the shortest that fits)
    desc->AddName(g_descriptor.name);

    // Category
    desc->AddCategory(g_descriptor.category);

    // Use monolithic topology (RenderAudio instead of algorithm callback)
    AAX_SInstrumentSetupInfo setupInfo = {};
    setupInfo.mManufacturerID = g_descriptor.manufacturer_id;
    setupInfo.mProductID = g_descriptor.product_id;
    setupInfo.mCanBypass = true;
    setupInfo.mUseHostGeneratedGUI = !g_descriptor.has_editor;

    if (g_descriptor.is_instrument) {
        setupInfo.mNeedsInputMIDI = true;
        setupInfo.mInputMIDINodeName = g_descriptor.name;
        setupInfo.mInputMIDIChannelMask = 0xFFFF; // all channels
    }

    // Register mono configuration
    setupInfo.mInputStemFormat = g_descriptor.num_inputs > 0
        ? AAX_eStemFormat_Mono : AAX_eStemFormat_Mono;
    setupInfo.mOutputStemFormat = AAX_eStemFormat_Mono;
    setupInfo.mPluginID = g_descriptor.plugin_id;
    AAX_Result err = AAX_CMonolithicParameters::StaticDescribe(desc, setupInfo);
    if (err != AAX_SUCCESS) return err;

    // Register stereo configuration (different plugin ID required)
    if (g_descriptor.num_outputs >= 2) {
        setupInfo.mInputStemFormat = g_descriptor.num_inputs >= 2
            ? AAX_eStemFormat_Stereo : AAX_eStemFormat_Mono;
        setupInfo.mOutputStemFormat = AAX_eStemFormat_Stereo;
        setupInfo.mPluginID = g_descriptor.plugin_id ^ 0x00000002; // unique ID for stereo
        err = AAX_CMonolithicParameters::StaticDescribe(desc, setupInfo);
        if (err != AAX_SUCCESS) return err;
    }

    // Register parameter class
    desc->AddProcPtr(
        (void*)TruceAAX_Parameters::Create,
        kAAX_ProcPtrID_Create_EffectParameters);

    // Register custom GUI if the plugin provides an editor
    if (g_descriptor.has_editor) {
        desc->AddProcPtr(
            (void*)TruceAAX_GUI::Create,
            kAAX_ProcPtrID_Create_EffectGUI);
    }

    // Add to collection
    outCollection->AddEffect(g_descriptor.name, desc);
    outCollection->SetManufacturerName(g_descriptor.vendor);
    outCollection->AddPackageName(g_descriptor.name);
    outCollection->SetPackageVersion(g_descriptor.version);

    return AAX_SUCCESS;
}
