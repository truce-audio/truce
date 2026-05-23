#include "TruceAAX_Parameters.h"

#include "AAX_CLinearTaperDelegate.h"
#include "AAX_CLogTaperDelegate.h"
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
// EffectInit - define parameters
// ---------------------------------------------------------------------------

AAX_Result TruceAAX_Parameters::EffectInit() {
    if (!g_bridge_loaded) return AAX_ERROR_NULL_OBJECT;

    // Create the Rust plugin instance
    mRustCtx = g_bridge.create();
    if (!mRustCtx) return AAX_ERROR_NULL_OBJECT;

    // Initialize plugin with sample rate. Pre-size for the
    // worst-case Pro Tools H/W buffer (8192 samples - the cap
    // exposed in the session settings, also used for offline
    // bounce). The plugin allocates internal scratch up to this
    // bound; per-block work in RenderAudio uses the actual
    // `*ioRenderInfo->mNumSamples`. If a host ever delivers a
    // larger block we re-reset there as a defensive fallback.
    AAX_CSampleRate sr = 44100.0;
    Controller()->GetSampleRate(&sr);
    mMaxBlockSize = 8192;
    g_bridge.reset(mRustCtx, (double)sr, mMaxBlockSize);
    mSampleRate = (double)sr;

    // Register parameters with AAX
    for (uint32_t i = 0; i < g_descriptor.num_params; i++) {
        TruceAaxParamInfo info = {};
        g_bridge.get_param_info(i, &info);

        // Pro Tools' master-bypass UI binds to the well-known
        // parameter ID `cDefaultMasterBypassID`. Use that string for
        // the IS_BYPASS-flagged param so the host's bypass button
        // tracks the param value; everything else gets `truce_p<id>`.
        AAX_CString paramID;
        if (info.id == g_descriptor.bypass_param_id) {
            paramID = cDefaultMasterBypassID;
        } else {
            std::ostringstream idStr;
            idStr << "truce_p" << info.id;
            paramID = AAX_CString(idStr.str().c_str());
        }

        // Pick the taper that matches Rust's `ParamRange` for this
        // param. AAX_CParameter stores the taper internally via
        // `Clone()`, so a stack-local instance per branch is fine -
        // the constructor copies before this scope ends. A
        // matched taper is what stops a log-ranged knob from
        // fighting the editor: with the default linear taper,
        // AAX's normalize/denormalize disagree with Rust's, and
        // the next render block writes back a different plain
        // value than the editor just stored.
        std::unique_ptr<AAX_IParameter> param;
        AAX_CUnitDisplayDelegateDecorator<float> display(
            AAX_CNumberDisplayDelegate<float>(),
            AAX_CString(info.unit));
        if (info.range_type == TRUCE_AAX_RANGE_LOG) {
            param.reset(new AAX_CParameter<float>(
                paramID,
                AAX_CString(info.name),
                (float)info.default_value,
                AAX_CLogTaperDelegate<float>((float)info.min, (float)info.max),
                display,
                true));
        } else {
            // Linear and Discrete both use a linear taper over
            // [min, max]; for Discrete the step quantization comes
            // from `SetNumberOfSteps` below, not the taper.
            param.reset(new AAX_CParameter<float>(
                paramID,
                AAX_CString(info.name),
                (float)info.default_value,
                AAX_CLinearTaperDelegate<float>((float)info.min, (float)info.max),
                display,
                true));
        }

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
// RenderAudio - main processing callback
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

    // Defensive: if the host violates the 8192-sample cap declared
    // in EffectInit, re-reset the plugin so its internal scratch
    // can fit the new block. This will glitch the audio - but a
    // glitch is recoverable; reading past the end of an allocated
    // buffer is not.
    if (bufferSize > 0 && (uint32_t)bufferSize > mMaxBlockSize) {
        mMaxBlockSize = (uint32_t)bufferSize;
        g_bridge.reset(mRustCtx, mSampleRate, mMaxBlockSize);
    }

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

    // Collect MIDI events for instruments and note effects (anything
    // that registered a LocalInput MIDI node in Describe). The cap
    // at 4096 packets per render block is far above any realistic
    // density (Pro Tools typically delivers tens to low hundreds
    // even for dense polyphonic recordings) and is stack-allocated
    // (4096 × 8 B = 32 KB) so the audio thread never heap-allocates.
    constexpr uint32_t kMidiBufferCap = 4096;
    TruceAaxMidiEvent midiEvents[kMidiBufferCap];
    uint32_t midiCount = 0;

    // Per-block SysEx reassembly scratch. AAX's `AAX_CMidiPacket`
    // holds up to 4 bytes; long `SysEx` messages span consecutive
    // packets framed by `0xF0` start / `0xF7` end (per `AAX.h:605`).
    // 64 KiB is enough for firmware-update-shaped payloads; longer
    // messages get truncated at the limit and dropped to keep the
    // audio thread allocation-free.
    constexpr uint32_t kSysExScratchCap = 64 * 1024;
    uint8_t sysexScratch[kSysExScratchCap];
    uint32_t sysexLen = 0;
    bool sysexInProgress = false;
    uint32_t sysexDeltaFrames = 0;
    bool sysexOverflowed = false;

    if (g_descriptor.wants_input_midi && ioRenderInfo->mInputNode) {
        AAX_IMIDINode* midiNode = ioRenderInfo->mInputNode;
        if (midiNode) {
            AAX_CMidiStream* stream = midiNode->GetNodeBuffer();
            if (stream && stream->mBufferSize > 0) {
                // Walk `pkt.mData[start..mLength]` as `SysEx` data
                // bytes. On `0xF7`: emit (if no overflow) and reset
                // state. On any other status byte: drop the
                // in-progress message and reset (mid-`SysEx` status
                // is a host spec violation; rest of the packet is
                // discarded). Otherwise accumulate into
                // `sysexScratch` until the cap is reached
                // (overflow flag latches so the eventual `0xF7` is
                // a drop, not a partial push).
                auto ingestSysexBytes = [&](const AAX_CMidiPacket& pkt, uint32_t start) {
                    for (uint32_t j = start; j < pkt.mLength; j++) {
                        uint8_t b = pkt.mData[j];
                        if (b == 0xF7) {
                            if (!sysexOverflowed && g_bridge.push_sysex_input) {
                                g_bridge.push_sysex_input(mRustCtx,
                                    sysexDeltaFrames, sysexScratch, sysexLen);
                            }
                            sysexInProgress = false;
                            sysexLen = 0;
                            sysexOverflowed = false;
                            return;
                        }
                        if (b & 0x80) {
                            sysexInProgress = false;
                            sysexLen = 0;
                            sysexOverflowed = false;
                            return;
                        }
                        if (sysexLen < kSysExScratchCap) {
                            sysexScratch[sysexLen++] = b;
                        } else {
                            sysexOverflowed = true;
                        }
                    }
                };

                for (uint32_t i = 0; i < stream->mBufferSize; i++) {
                    const AAX_CMidiPacket& pkt = stream->mBuffer[i];
                    if (pkt.mLength < 1) continue;
                    const uint8_t status = pkt.mData[0];
                    // SysEx start: status `0xF0` in the first byte of
                    // a packet. Subsequent packets carry only data
                    // bytes until a `0xF7` terminator. Per the AAX
                    // SDK each `AAX_CMidiPacket` is one message - so
                    // once we enter either `SysEx` branch the whole
                    // packet belongs to it (channel-voice is the
                    // explicit `else`).
                    if (sysexInProgress) {
                        ingestSysexBytes(pkt, 0);
                        continue;
                    }
                    if (status == 0xF0) {
                        sysexInProgress = true;
                        sysexLen = 0;
                        sysexOverflowed = false;
                        sysexDeltaFrames = pkt.mTimestamp;
                        // Single-packet `SysEx` (ends with `0xF7` in
                        // the same packet) is handled by
                        // `ingestSysexBytes` discovering `0xF7` in
                        // its walk.
                        ingestSysexBytes(pkt, 1);
                        continue;
                    }
                    if (midiCount < kMidiBufferCap) {
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

    // Drain plugin-emitted MIDI to the host. The component descriptor
    // built in `TruceAAX_Describe.cpp` registered an extra `LocalOutput`
    // MIDI node past the end of `AAX_SInstrumentRenderInfo`; recover it
    // by casting `ioRenderInfo` back to the extended struct that the
    // runtime actually populates (the cast is sound - same offsets for
    // the inherited fields, plus one extra slot for `mOutputNode`).
    auto* extendedInfo = reinterpret_cast<TruceAaxExtendedRenderInfo*>(ioRenderInfo);
    AAX_IMIDINode* outputNode = extendedInfo->mOutputNode;
    if (outputNode) {
        uint32_t outCount = g_bridge.output_event_count(mRustCtx);
        for (uint32_t i = 0; i < outCount; i++) {
            TruceAaxMidiEvent ev = {};
            g_bridge.output_event_at(mRustCtx, i, &ev);
            AAX_CMidiPacket pkt = {};
            pkt.mTimestamp = ev.delta_frames;
            pkt.mLength = 3;
            pkt.mData[0] = ev.status;
            pkt.mData[1] = ev.data1;
            pkt.mData[2] = ev.data2;
            // Two-byte messages (Program Change, Channel Pressure)
            // ignore mData[2]; the SDK packet carries up to 4 bytes
            // and Pro Tools reads only mLength bytes regardless.
            const uint8_t st = ev.status & 0xF0;
            if (st == 0xC0 || st == 0xD0) pkt.mLength = 2;
            outputNode->PostMIDIPacket(&pkt);
        }

        // Output SysEx: per AAX_Enums.h:1160, "There are no buffer
        // size limitations for output of SysEx messages." We frame
        // each event (`0xF0` + inner bytes + `0xF7`) and fragment
        // into a sequence of ≤4-byte packets sharing one timestamp.
        if (g_bridge.output_sysex_count) {
            uint32_t sxCount = g_bridge.output_sysex_count(mRustCtx);
            for (uint32_t i = 0; i < sxCount; i++) {
                uint32_t delta = 0;
                const uint8_t* bytes = nullptr;
                uint32_t len = 0;
                g_bridge.output_sysex_at(mRustCtx, i, &delta, &bytes, &len);
                if (!bytes && len > 0) continue;

                // Build framed stream on the fly without buffering
                // the whole message - chunk loop reads the next byte
                // from one of three sources (start marker, payload,
                // end marker) based on position.
                uint32_t totalLen = len + 2;     // +2 for 0xF0 / 0xF7
                uint32_t pos = 0;
                auto byteAt = [&](uint32_t p) -> uint8_t {
                    if (p == 0)              return 0xF0;
                    if (p == totalLen - 1)   return 0xF7;
                    return bytes[p - 1];
                };
                while (pos < totalLen) {
                    AAX_CMidiPacket pkt = {};
                    pkt.mTimestamp = delta;
                    uint32_t chunk = totalLen - pos;
                    if (chunk > 4) chunk = 4;
                    pkt.mLength = chunk;
                    for (uint32_t j = 0; j < chunk; j++) {
                        pkt.mData[j] = byteAt(pos + j);
                    }
                    outputNode->PostMIDIPacket(&pkt);
                    pos += chunk;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// State (chunk) support
// ---------------------------------------------------------------------------

// The AAX standard control chunk ID. `AAX_CEffectParameters`'s
// `GetChunkIDFromIndex(0)` already returns this value from the
// SDK's default implementation, so we just need our chunk
// handlers to honor the same gate. Without the gate, every chunk
// Pro Tools probes for (preset 'pset', mode 'mode', ...) gets
// our `save_state` blob, and the host's chunk table fills with
// wrong-sized entries — `SMgr_PlugInInst::GetLiveSettings` then
// trips a size-mismatch assertion when two plugins share a
// track. Refusing unknown chunks via `AAX_ERROR_INVALID_CHUNK_ID`
// keeps the host's chunk table truthful.
constexpr AAX_CTypeID kTruceControlsChunkID = 'elck';

AAX_Result TruceAAX_Parameters::GetChunkSize(AAX_CTypeID chunkID, uint32_t* oSize) const {
    if (!mRustCtx || !g_bridge_loaded) return AAX_ERROR_NULL_OBJECT;
    if (chunkID != kTruceControlsChunkID) {
        *oSize = 0;
        return AAX_ERROR_INVALID_CHUNK_ID;
    }
    // Serialize once into the pending cache; GetChunk drains it.
    uint8_t* data = nullptr;
    uint32_t len = g_bridge.save_state(mRustCtx, &data);
    mPendingChunk.assign(data, data + len);
    if (data) g_bridge.free_state(data, len);
    *oSize = len;
    return AAX_SUCCESS;
}

AAX_Result TruceAAX_Parameters::GetChunk(AAX_CTypeID chunkID, AAX_SPlugInChunk* oChunk) const {
    if (!mRustCtx || !g_bridge_loaded) return AAX_ERROR_NULL_OBJECT;
    if (chunkID != kTruceControlsChunkID) {
        return AAX_ERROR_INVALID_CHUNK_ID;
    }
    // Prefer the blob cached by the immediately-preceding GetChunkSize
    // call. Fall back to a fresh serialize only if Pro Tools violates
    // the usual size-then-copy contract (defensive - shouldn't happen).
    if (mPendingChunk.empty()) {
        uint8_t* data = nullptr;
        uint32_t len = g_bridge.save_state(mRustCtx, &data);
        if (data) {
            mPendingChunk.assign(data, data + len);
            g_bridge.free_state(data, len);
        }
    }
    if (!mPendingChunk.empty() && mPendingChunk.size() <= oChunk->fSize) {
        memcpy(oChunk->fData, mPendingChunk.data(), mPendingChunk.size());
        oChunk->fSize = (uint32_t)mPendingChunk.size();
    }
    mPendingChunk.clear();
    mPendingChunk.shrink_to_fit();
    return AAX_SUCCESS;
}

AAX_Result TruceAAX_Parameters::SetChunk(AAX_CTypeID chunkID, const AAX_SPlugInChunk* iChunk) {
    if (!mRustCtx || !g_bridge_loaded) return AAX_ERROR_NULL_OBJECT;
    if (chunkID != kTruceControlsChunkID) {
        return AAX_ERROR_INVALID_CHUNK_ID;
    }
    g_bridge.load_state(mRustCtx, (const uint8_t*)iChunk->fData, iChunk->fSize);
    return AAX_SUCCESS;
}
