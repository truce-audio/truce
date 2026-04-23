#pragma once

#include "AAX_CMonolithicParameters.h"
#include "TruceAAX_Bridge.h"

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

    // State
    AAX_Result GetChunkSize(AAX_CTypeID chunkID, uint32_t* oSize) const override;
    AAX_Result GetChunk(AAX_CTypeID chunkID, AAX_SPlugInChunk* oChunk) const override;
    AAX_Result SetChunk(AAX_CTypeID chunkID, const AAX_SPlugInChunk* iChunk) override;

    void* GetRustCtx() const { return mRustCtx; }

private:
    void* mRustCtx;
    std::vector<uint32_t> mParamIDs; // maps AAX index → truce param ID
    // Cache for a single GetChunkSize → GetChunk pair. Pro Tools calls
    // GetChunkSize before GetChunk to size the buffer; without this
    // cache we'd serialize the full state blob twice per save.
    // `mutable` because both methods are const per the AAX SDK.
    mutable std::vector<uint8_t> mPendingChunk;
};
