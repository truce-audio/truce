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
#include "AAX_CMonolithicParameters.h"
#include "AAX_Enums.h"

#include <cstdio>
#include <cstddef>

// Build the component descriptor by hand for one stem config.
//
// Replaces `AAX_CMonolithicParameters::StaticDescribe` so we can
// register a `LocalOutput` MIDI node (plugin → host MIDI), which the
// SDK helper doesn't expose. The body mirrors `StaticDescribe`'s logic
// from `AAX_CMonolithicParameters.cpp` field-for-field, swapping
// `AAX_FIELD_INDEX(AAX_SInstrumentRenderInfo, ...)` for
// `AAX_FIELD_INDEX(TruceAaxExtendedRenderInfo, base.<member>)` so the
// runtime fills the same offsets in the larger context block, and
// adds one extra `AddMIDINode` call for `mOutputNode` past the end of
// the base struct. The render proc remains
// `AAX_CMonolithicParameters::StaticRenderAudio`, which only reads
// `mPrivateData->mMonolithicParametersPtr` — at the unchanged offset
// — and dispatches to `parameters->RenderAudio(*ptr, ...)`. The cast
// of `*ptr` back to `TruceAaxExtendedRenderInfo*` inside `RenderAudio`
// is what gives the parameters object access to `mOutputNode`.
static AAX_Result TruceDescribeOneConfig(
    AAX_IEffectDescriptor* ioDescriptor,
    const AAX_SInstrumentSetupInfo& setupInfo,
    bool needsOutputMIDI,
    const char* outputMIDIName)
{
    AAX_IComponentDescriptor* const compDesc = ioDescriptor->NewComponentDescriptor();
    if (!compDesc) return AAX_ERROR_NULL_OBJECT;

    AAX_Result err = AAX_SUCCESS;

    const AAX_CFieldIndex globalNodeID = AAX_FIELD_INDEX(TruceAaxExtendedRenderInfo, base.mGlobalNode);
    const AAX_CFieldIndex localInputNodeID = AAX_FIELD_INDEX(TruceAaxExtendedRenderInfo, base.mInputNode);
    const AAX_CFieldIndex transportNodeID = AAX_FIELD_INDEX(TruceAaxExtendedRenderInfo, base.mTransportNode);
    const AAX_CFieldIndex outputNodeID = AAX_FIELD_INDEX(TruceAaxExtendedRenderInfo, mOutputNode);

    if (setupInfo.mNeedsGlobalMIDI)
        err = compDesc->AddMIDINode(globalNodeID, AAX_eMIDINodeType_Global,
                                    setupInfo.mGlobalMIDINodeName, setupInfo.mGlobalMIDIEventMask);
    else
        err = compDesc->AddPrivateData(globalNodeID, sizeof(float),
                                        AAX_ePrivateDataOptions_DefaultOptions);
    if (err != AAX_SUCCESS) return err;

    if (setupInfo.mNeedsInputMIDI)
        err = compDesc->AddMIDINode(localInputNodeID, AAX_eMIDINodeType_LocalInput,
                                    setupInfo.mInputMIDINodeName, setupInfo.mInputMIDIChannelMask);
    else
        err = compDesc->AddPrivateData(localInputNodeID, sizeof(float),
                                        AAX_ePrivateDataOptions_DefaultOptions);
    if (err != AAX_SUCCESS) return err;

    if (setupInfo.mNeedsTransport)
        err = compDesc->AddMIDINode(transportNodeID, AAX_eMIDINodeType_Transport,
                                    "Transport", 0xffff);
    else
        err = compDesc->AddPrivateData(transportNodeID, sizeof(float),
                                        AAX_ePrivateDataOptions_DefaultOptions);
    if (err != AAX_SUCCESS) return err;

    // Plugin → host MIDI. Pro Tools posts each
    // `AAX_IMIDINode::PostMIDIPacket` call from the parameters object's
    // `RenderAudio` to subscribers of this output node; the channel
    // mask isn't honored on output (Pro Tools accepts whatever channel
    // the packet's status byte carries), but the SDK still requires
    // *some* mask be set — `0xFFFF` matches the input-MIDI default.
    if (needsOutputMIDI) {
        err = compDesc->AddMIDINode(outputNodeID, AAX_eMIDINodeType_LocalOutput,
                                    outputMIDIName, 0xFFFF);
    } else {
        err = compDesc->AddPrivateData(outputNodeID, sizeof(float),
                                        AAX_ePrivateDataOptions_DefaultOptions);
    }
    if (err != AAX_SUCCESS) return err;

    // Skip the additional input MIDI nodes (mNumAdditionalInputMIDINodes
    // is always 0 for truce — we don't expose multi-port-input plugins).

    err = compDesc->AddAudioIn(AAX_FIELD_INDEX(TruceAaxExtendedRenderInfo, base.mAudioInputs));
    if (err != AAX_SUCCESS) return err;
    err = compDesc->AddAudioOut(AAX_FIELD_INDEX(TruceAaxExtendedRenderInfo, base.mAudioOutputs));
    if (err != AAX_SUCCESS) return err;
    err = compDesc->AddAudioBufferLength(AAX_FIELD_INDEX(TruceAaxExtendedRenderInfo, base.mNumSamples));
    if (err != AAX_SUCCESS) return err;
    err = compDesc->AddClock(AAX_FIELD_INDEX(TruceAaxExtendedRenderInfo, base.mClock));
    if (err != AAX_SUCCESS) return err;

    // No meters declared — fill the slot with a small block of private
    // data so the offset stays reserved (matches `StaticDescribe`'s
    // behavior for `mNumMeters == 0`).
    err = compDesc->AddPrivateData(AAX_FIELD_INDEX(TruceAaxExtendedRenderInfo, base.mMeters),
                                    sizeof(float), AAX_ePrivateDataOptions_DefaultOptions);
    if (err != AAX_SUCCESS) return err;

    err = compDesc->AddPrivateData(AAX_FIELD_INDEX(TruceAaxExtendedRenderInfo, base.mPrivateData),
                                    sizeof(AAX_SInstrumentPrivateData),
                                    AAX_ePrivateDataOptions_DefaultOptions);
    if (err != AAX_SUCCESS) return err;

    err = compDesc->AddDataInPort(AAX_FIELD_INDEX(TruceAaxExtendedRenderInfo, base.mCurrentStateNum),
                                   sizeof(uint64_t));
    if (err != AAX_SUCCESS) return err;

    AAX_IPropertyMap* const properties = compDesc->NewPropertyMap();
    if (!properties) return AAX_ERROR_NULL_OBJECT;

    if (setupInfo.mUseHostGeneratedGUI)
        err = properties->AddProperty(AAX_eProperty_UsesClientGUI, true);
    err = properties->AddProperty(AAX_eProperty_InputStemFormat,
                                   static_cast<int32_t>(setupInfo.mInputStemFormat));
    err = properties->AddProperty(AAX_eProperty_OutputStemFormat,
                                   static_cast<int32_t>(setupInfo.mOutputStemFormat));
    err = properties->AddProperty(AAX_eProperty_CanBypass, setupInfo.mCanBypass);
    err = properties->AddProperty(AAX_eProperty_Constraint_Location,
                                   0x0 | AAX_eConstraintLocationMask_DataModel);
    if (setupInfo.mNeedsTransport)
        err = properties->AddProperty(AAX_eProperty_UsesTransport, true);
    err = properties->AddProperty(AAX_eProperty_ManufacturerID,
                                   static_cast<int32_t>(setupInfo.mManufacturerID));
    err = properties->AddProperty(AAX_eProperty_ProductID,
                                   static_cast<int32_t>(setupInfo.mProductID));
    err = properties->AddProperty(AAX_eProperty_PlugInID_Native,
                                   static_cast<int32_t>(setupInfo.mPluginID));
    if (!setupInfo.mMultiMonoSupport)
        err = properties->AddProperty(AAX_eProperty_Constraint_MultiMonoSupport, 0);

    err = compDesc->AddProcessProc_Native(
        AAX_CMonolithicParameters::StaticRenderAudio, properties);
    if (err != AAX_SUCCESS) return err;

    return ioDescriptor->AddComponent(compDesc);
}

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

    // Instruments AND note effects (MIDI processors) need a LocalInput
    // MIDI node so Pro Tools delivers note events into the plugin.
    // Without this, transpose/arpeggio would never see input notes and
    // produce nothing on their MIDI output.
    if (g_descriptor.wants_input_midi) {
        setupInfo.mNeedsInputMIDI = true;
        setupInfo.mInputMIDINodeName = g_descriptor.name;
        setupInfo.mInputMIDIChannelMask = 0xFFFF; // all channels
    }

    // Register mono configuration. The Rust wrapper synthesizes
    // dummy stereo I/O for plugins that declare no audio buses
    // (pure MIDI effects, instruments) — see the layout match in
    // `truce-aax/src/lib.rs::register_aax` — so `num_inputs > 0`
    // always holds at this point and `Mono` is the right choice.
    setupInfo.mInputStemFormat = AAX_eStemFormat_Mono;
    setupInfo.mOutputStemFormat = AAX_eStemFormat_Mono;
    setupInfo.mPluginID = g_descriptor.plugin_id;
    AAX_Result err = TruceDescribeOneConfig(desc, setupInfo,
                                             /*needsOutputMIDI=*/true,
                                             g_descriptor.name);
    if (err != AAX_SUCCESS) return err;

    // Register stereo configuration (different plugin ID required)
    if (g_descriptor.num_outputs >= 2) {
        setupInfo.mInputStemFormat = g_descriptor.num_inputs >= 2
            ? AAX_eStemFormat_Stereo : AAX_eStemFormat_Mono;
        setupInfo.mOutputStemFormat = AAX_eStemFormat_Stereo;
        setupInfo.mPluginID = g_descriptor.plugin_id ^ 0x00000002; // unique ID for stereo
        err = TruceDescribeOneConfig(desc, setupInfo,
                                      /*needsOutputMIDI=*/true,
                                      g_descriptor.name);
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
