#include "TruceAAX_Parameters.h"

#include "AAX_CLinearTaperDelegate.h"
#include "AAX_CNumberDisplayDelegate.h"
#include "AAX_CUnitDisplayDelegateDecorator.h"
#include "AAX_CBinaryTaperDelegate.h"
#include "AAX_CBinaryDisplayDelegate.h"
#include "AAX_IMIDINode.h"
#include "AAX_IController.h"
#include "AAX_ITransport.h"

#include <cstring>
#include <cstdio>
#include <sstream>
#include <memory>

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

AAX_CEffectParameters* AAX_CALLBACK TruceAAX_Parameters::Create() {
    return new TruceAAX_Parameters();
}

// ---------------------------------------------------------------------------
// Constructor / Destructor
// ---------------------------------------------------------------------------

TruceAAX_Parameters::TruceAAX_Parameters()
    : AAX_CMonolithicParameters()
    , mRustCtx(nullptr) {
}

TruceAAX_Parameters::~TruceAAX_Parameters() {
    if (mRustCtx && g_bridge_loaded) {
        g_bridge.destroy(mRustCtx);
        mRustCtx = nullptr;
    }
}

// ---------------------------------------------------------------------------
// EffectInit — define parameters
// ---------------------------------------------------------------------------

AAX_Result TruceAAX_Parameters::EffectInit() {
    if (!g_bridge_loaded) return AAX_ERROR_NULL_OBJECT;

    // Create the Rust plugin instance
    mRustCtx = g_bridge.create();
    if (!mRustCtx) return AAX_ERROR_NULL_OBJECT;

    // Initialize plugin with sample rate
    AAX_CSampleRate sr = 44100.0;
    Controller()->GetSampleRate(&sr);
    g_bridge.reset(mRustCtx, (double)sr, 1024);

    // Register parameters with AAX
    for (uint32_t i = 0; i < g_descriptor.num_params; i++) {
        TruceAaxParamInfo info = {};
        g_bridge.get_param_info(i, &info);

        std::ostringstream idStr;
        idStr << "truce_p" << info.id;
        AAX_CString paramID(idStr.str().c_str());

        auto param = std::unique_ptr<AAX_IParameter>(new AAX_CParameter<float>(
            paramID,
            AAX_CString(info.name),
            (float)info.default_value,
            AAX_CLinearTaperDelegate<float>((float)info.min, (float)info.max),
            AAX_CUnitDisplayDelegateDecorator<float>(
                AAX_CNumberDisplayDelegate<float>(),
                AAX_CString(info.unit)),
            true));  // automatable

        param->SetNumberOfSteps(info.step_count > 0 ? info.step_count : 128);
        param->SetType(AAX_eParameterType_Continuous);

        AAX_IParameter* rawParam = param.release();
        mParameterManager.AddParameter(rawParam);
        AddSynchronizedParameter(*rawParam);
        mParamIDs.push_back(info.id);
    }

    return AAX_SUCCESS;
}

// ---------------------------------------------------------------------------
// RenderAudio — main processing callback
// ---------------------------------------------------------------------------

void TruceAAX_Parameters::RenderAudio(
    AAX_SInstrumentRenderInfo* ioRenderInfo,
    const TParamValPair* inSynchronizedParamValues[],
    int32_t inNumSynchronizedParamValues)
{
    if (!mRustCtx || !g_bridge_loaded) return;

    // Sync parameter values from AAX to Rust
    for (int32_t i = 0; i < inNumSynchronizedParamValues; i++) {
        const TParamValPair& pv = *inSynchronizedParamValues[i];
        // Extract param index from ID string "truce_pN"
        const char* idStr = pv.first;
        if (strncmp(idStr, "truce_p", 7) == 0) {
            uint32_t id = (uint32_t)atoi(idStr + 7);
            float val;
            if (pv.second && pv.second->GetValueAsFloat(&val))
                g_bridge.set_param(mRustCtx, id, (double)val);
        }
    }

    // Get audio buffers
    int32_t bufferSize = *ioRenderInfo->mNumSamples;

    // Build channel pointers
    const float* inputs[2] = { nullptr, nullptr };
    float* outputs[2] = { nullptr, nullptr };

    if (g_descriptor.num_inputs > 0 && ioRenderInfo->mAudioInputs) {
        for (uint32_t ch = 0; ch < g_descriptor.num_inputs && ch < 2; ch++)
            inputs[ch] = ioRenderInfo->mAudioInputs[ch];
    }
    if (ioRenderInfo->mAudioOutputs) {
        for (uint32_t ch = 0; ch < g_descriptor.num_outputs && ch < 2; ch++)
            outputs[ch] = ioRenderInfo->mAudioOutputs[ch];
    }

    // Collect MIDI events (for instruments)
    TruceAaxMidiEvent midiEvents[256];
    uint32_t midiCount = 0;

    if (g_descriptor.is_instrument && ioRenderInfo->mInputNode) {
        AAX_IMIDINode* midiNode = ioRenderInfo->mInputNode;
        if (midiNode) {
            AAX_CMidiStream* stream = midiNode->GetNodeBuffer();
            if (stream && stream->mBufferSize > 0) {
                for (uint32_t i = 0; i < stream->mBufferSize && midiCount < 256; i++) {
                    const AAX_CMidiPacket& pkt = stream->mBuffer[i];
                    if (pkt.mLength >= 1) {
                        midiEvents[midiCount].delta_frames = pkt.mTimestamp;
                        midiEvents[midiCount].status = pkt.mData[0];
                        midiEvents[midiCount].data1 = pkt.mLength > 1 ? pkt.mData[1] : 0;
                        midiEvents[midiCount].data2 = pkt.mLength > 2 ? pkt.mData[2] : 0;
                        midiEvents[midiCount]._pad = 0;
                        midiCount++;
                    }
                }
            }
        }
    }

    // Query Pro Tools transport. Each getter is independent so the
    // snapshot remains useful even if the host only answers some of
    // them. All coordinates come back in beats / ticks / samples and
    // are forwarded verbatim to Rust.
    TruceAaxTransportSnapshot transport = {};
    AAX_ITransport* trans = Transport();
    if (trans) {
        bool playing = false;
        if (trans->IsTransportPlaying(&playing) == AAX_SUCCESS) {
            transport.playing = playing ? 1 : 0;
            transport.valid = 1;
        }
        double tempo = 0.0;
        if (trans->GetCurrentTempo(&tempo) == AAX_SUCCESS && tempo > 0.0) {
            transport.tempo = tempo;
            transport.valid = 1;
        }
        int32_t num = 0, den = 0;
        if (trans->GetCurrentMeter(&num, &den) == AAX_SUCCESS) {
            transport.time_sig_num = num;
            transport.time_sig_den = den;
            transport.valid = 1;
        }
        int64_t sampleLoc = 0;
        if (trans->GetCurrentNativeSampleLocation(&sampleLoc) == AAX_SUCCESS) {
            transport.position_samples = (double)sampleLoc;
            transport.valid = 1;
        }
        int64_t tickPos = 0;
        if (trans->GetCurrentTickPosition(&tickPos) == AAX_SUCCESS) {
            // AAX ticks are 1/960000 of a quarter note; convert to beats.
            transport.position_beats = (double)tickPos / 960000.0;
            transport.valid = 1;
        }
        // Bar/beat at the current sample location. GetBarBeatPosition
        // returns zero-based bar + beat indices; convert to a beat
        // count by multiplying bars by the reported meter numerator.
        int32_t bars = 0, beats = 0;
        int64_t barDisplayTicks = 0;
        int64_t samplePos = (int64_t)transport.position_samples;
        if (trans->GetBarBeatPosition(&bars, &beats, &barDisplayTicks, samplePos)
                == AAX_SUCCESS
            && transport.time_sig_num > 0)
        {
            transport.bar_start_beats =
                (double)bars * (double)transport.time_sig_num;
            transport.valid = 1;
        }
        bool loop = false;
        int64_t loopStart = 0, loopEnd = 0;
        if (trans->GetCurrentLoopPosition(&loop, &loopStart, &loopEnd) == AAX_SUCCESS) {
            transport.loop_active = loop ? 1 : 0;
            transport.loop_start_beats = (double)loopStart / 960000.0;
            transport.loop_end_beats = (double)loopEnd / 960000.0;
            transport.valid = 1;
        }
    }

    // Call the Rust processing function
    g_bridge.process(mRustCtx,
        inputs, outputs,
        g_descriptor.num_inputs, g_descriptor.num_outputs,
        (uint32_t)bufferSize,
        midiEvents, midiCount,
        transport.valid ? &transport : nullptr);
}

// ---------------------------------------------------------------------------
// State (chunk) support
// ---------------------------------------------------------------------------

AAX_Result TruceAAX_Parameters::GetChunkSize(AAX_CTypeID chunkID, uint32_t* oSize) const {
    if (!mRustCtx || !g_bridge_loaded) return AAX_ERROR_NULL_OBJECT;
    uint8_t* data = nullptr;
    *oSize = g_bridge.save_state(mRustCtx, &data);
    if (data) g_bridge.free_state(data, *oSize);
    return AAX_SUCCESS;
}

AAX_Result TruceAAX_Parameters::GetChunk(AAX_CTypeID chunkID, AAX_SPlugInChunk* oChunk) const {
    if (!mRustCtx || !g_bridge_loaded) return AAX_ERROR_NULL_OBJECT;
    uint8_t* data = nullptr;
    uint32_t len = g_bridge.save_state(mRustCtx, &data);
    if (data && len > 0 && len <= oChunk->fSize) {
        memcpy(oChunk->fData, data, len);
        oChunk->fSize = len;
        g_bridge.free_state(data, len);
    }
    return AAX_SUCCESS;
}

AAX_Result TruceAAX_Parameters::SetChunk(AAX_CTypeID chunkID, const AAX_SPlugInChunk* iChunk) {
    if (!mRustCtx || !g_bridge_loaded) return AAX_ERROR_NULL_OBJECT;
    g_bridge.load_state(mRustCtx, (const uint8_t*)iChunk->fData, iChunk->fSize);
    return AAX_SUCCESS;
}
