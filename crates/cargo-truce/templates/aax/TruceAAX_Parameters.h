#pragma once

#include "AAX_CMonolithicParameters.h"
#include "AAX_IMIDINode.h"
#include "TruceAAX_Bridge.h"

// Extended render-info struct used by TruceAAX_Describe.cpp's hand-built
// component descriptor.
//
// `AAX_CMonolithicParameters::StaticDescribe` registers the standard
// `AAX_SInstrumentRenderInfo` field set, which has slots for input /
// global / transport MIDI nodes but **no** slot for a `LocalOutput`
// node - the SDK's monolithic-parameters helper simply doesn't expose
// plugin → host MIDI through `AAX_SInstrumentSetupInfo`. We replicate
// the body of `StaticDescribe` inline (`Describe.cpp`) and append one
// more port: `mOutputNode` at the end of this struct, registered via
// `compDesc->AddMIDINode(..., AAX_eMIDINodeType_LocalOutput, ...)`.
//
// The `base` member's layout is identical to a freestanding
// `AAX_SInstrumentRenderInfo`, so passing `&extended->base` to the
// inherited `RenderAudio(AAX_SInstrumentRenderInfo*)` virtual is sound
// - Pro Tools fills the slots by offset, and the offsets of all
// inherited fields match. The extra `mOutputNode` slot lives past the
// end of `base` and is read inside `RenderAudio` by casting back up to
// `TruceAaxExtendedRenderInfo*`.
struct TruceAaxExtendedRenderInfo {
    AAX_SInstrumentRenderInfo base;
    AAX_IMIDINode* mOutputNode;
};

class TruceAAX_Parameters : public AAX_CMonolithicParameters {
public:
    TruceAAX_Parameters();
    ~TruceAAX_Parameters() override;

    static AAX_CEffectParameters* AAX_CALLBACK Create();

    AAX_Result EffectInit() override;

    void RenderAudio(
        AAX_SInstrumentRenderInfo* ioRenderInfo,
        const TParamValPair* inSynchronizedParamValues[],
        int32_t inNumSynchronizedParamValues) override;

    // Offline-bounce awareness: Pro Tools posts EnteringOfflineMode /
    // ExitingOfflineMode here; we forward the render mode to Rust so
    // reset / process observe the right ProcessMode.
    AAX_Result NotificationReceived(AAX_CTypeID inNotificationType,
                                    const void* inNotificationData,
                                    uint32_t inNotificationDataSize) override;

    // Dynamic latency: the host drives this idle callback (~30 ms). We
    // poll the plugin's current latency and push changes to the host via
    // AAX_IController::SetSignalLatency - off the audio thread.
    AAX_Result TimerWakeup() override;

    // State
    AAX_Result GetChunkSize(AAX_CTypeID chunkID, uint32_t* oSize) const override;
    AAX_Result GetChunk(AAX_CTypeID chunkID, AAX_SPlugInChunk* oChunk) const override;
    AAX_Result SetChunk(AAX_CTypeID chunkID, const AAX_SPlugInChunk* iChunk) override;
    // Declared so Pro Tools hands old sessions' legacy chunks
    // (descriptor legacy_chunk_ids) to SetChunk for migration.
    AAX_Result GetNumberOfChunks(int32_t* oNumChunks) const override;
    AAX_Result GetChunkIDFromIndex(int32_t index, AAX_CTypeID* oChunkID) const override;

    void* GetRustCtx() const { return mRustCtx; }

private:
    // Push the plugin's current latency to the host when it changed
    // since the last push. Idle-thread only (calls the controller).
    void PushLatencyIfChanged();

    void* mRustCtx;
    std::vector<uint32_t> mParamIDs; // maps AAX index → truce param ID
    // Cache for a single GetChunkSize → GetChunk pair. Pro Tools calls
    // GetChunkSize before GetChunk to size the buffer; without this
    // cache we'd serialize the full state blob twice per save.
    // `mutable` because both methods are const per the AAX SDK.
    mutable std::vector<uint8_t> mPendingChunk;
    // Sample rate captured at EffectInit; used by RenderAudio's
    // defensive re-reset path when a host delivers a block larger
    // than `mMaxBlockSize`.
    double mSampleRate = 44100.0;
    uint32_t mMaxBlockSize = 0;
    // Channel counts of the stem format this instance was instantiated
    // with, read from the controller in EffectInit. A plugin declaring
    // multiple bus_layouts() gets one component (hence instance) per
    // layout, so these vary per instance - RenderAudio must wire exactly
    // this many host pointers, not the descriptor's first-layout count.
    uint32_t mNumInputChannels = 0;
    uint32_t mNumOutputChannels = 0;
    // Last latency (samples) pushed to the host via SetSignalLatency.
    // -1 sentinel so the first TimerWakeup always reports, even if the
    // plugin's latency is 0. Touched only on the host idle thread.
    int32_t mLastReportedLatency = -1;
};
