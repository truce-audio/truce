/// AU v3 Swift implementation - delegates all plugin logic to the Rust
/// framework via C FFI (g_callbacks function pointer table).
import os.log
import AudioToolbox
import CoreMIDI

private let logger = Logger(subsystem: "com.truce.au3", category: "AUExt")
import AVFAudio
import CoreAudioKit

#if os(iOS)
import UIKit
// AppKit `NSView` / `NSSize` / `NSRect` are macOS-only. The iOS
// AUv3 view-controller hosts a UIView; the helpers below alias
// the AppKit types to their UIKit equivalents so most of the
// factory code stays platform-agnostic.
typealias NSView = UIView
typealias NSSize = CGSize
typealias NSRect = CGRect
#else
import AppKit
#endif

// MARK: - UMP helpers

/// `AURenderEventMIDIEventList` raw value from
/// `AudioToolbox/AudioUnitProperties.h`. Compared against
/// `head.eventType.rawValue` instead of a Swift enum case so older
/// Xcode versions (where the case isn't yet imported) still
/// compile. The enum value is stable per Apple's header.
let kAURenderEventMIDIEventListRaw: UInt16 = 10

/// UMP packet length in 32-bit words by message type. Spec: MIDI
/// 2.0 M2-104-UM, §2.1.4 (Message Type field).
@inline(__always) func umpPacketLength(messageType mt: UInt8) -> Int {
    switch mt {
    case 0x0, 0x1, 0x2: return 1            // utility, system, MIDI 1.0 CV
    case 0x3, 0x4: return 2                  // SysEx-7, MIDI 2.0 CV
    case 0x5, 0xD, 0xE, 0xF: return 4        // data 128, flex, UMP stream
    default: return 1
    }
}

/// Read the `MIDIEventsList` slot of an `AURenderEvent` and forward
/// every UMP it carries into the appropriate AuMidi(2)Event buffer.
/// Returns `(midi1_added, midi2_added)`. Pointer-based: avoids the
/// Swift overlay's `.midiEventList` discriminator which isn't
/// exposed on every Xcode version we want to support.
func forwardMIDIEventList(
    event: UnsafePointer<AURenderEvent>,
    bufStart: Int64,
    frameCount: UInt32,
    midiBuf: UnsafeMutablePointer<AuMidiEvent>, midiStart: UInt32,
    midi2Buf: UnsafeMutablePointer<AuMidi2Event>, midi2Start: UInt32
) -> (UInt32, UInt32) {
    // AURenderEvent is a tagged union; reach into the
    // `MIDIEventsList` slot via raw pointer arithmetic so the code
    // compiles on Xcodes that haven't yet imported the symbol.
    // Layout: AURenderEventHeader (16 bytes) + AUMIDIEventList payload.
    // AUMIDIEventList: { AURenderEventHeader head; uint64_t
    //                    eventSampleTime; MIDIEventList eventList; }
    let raw = UnsafeRawPointer(event)
    let payload = raw.advanced(by: 16)
    let absTime = payload.assumingMemoryBound(to: Int64.self).pointee
    let relOffset = max(0, absTime - bufStart)
    let offset = UInt32(min(relOffset, Int64(frameCount - 1)))
    // MIDIEventList layout (CoreMIDI/MIDIServices.h):
    //   MIDIProtocolID protocol; uint32_t numPackets;
    //   MIDIEventPacket packet[1];  // variable-length tail
    let listBase = payload.advanced(by: 8)
    let proto = listBase.assumingMemoryBound(to: UInt32.self).pointee
    let numPackets = listBase.advanced(by: 4).assumingMemoryBound(to: UInt32.self).pointee
    // Protocol 1 = MIDI 1.0 UMP, 2 = MIDI 2.0 UMP. We accept both.
    _ = proto
    var pktBase = listBase.advanced(by: 8) // start of first packet
    var midiCount: UInt32 = midiStart
    var midi2Count: UInt32 = midi2Start
    var packetIdx: UInt32 = 0
    while packetIdx < numPackets && midiCount < 256 && midi2Count < 256 {
        // MIDIEventPacket: { MIDITimeStamp timeStamp; uint32_t
        // wordCount; uint32_t words[64]; }
        let wordCount = pktBase.advanced(by: 8).assumingMemoryBound(to: UInt32.self).pointee
        let wordsPtr = pktBase.advanced(by: 12).assumingMemoryBound(to: UInt32.self)
        var i: UInt32 = 0
        while i < wordCount && midiCount < 256 && midi2Count < 256 {
            let w0 = (wordsPtr + Int(i)).pointee
            let mt = UInt8((w0 >> 28) & 0xF)
            let packetWords = UInt32(umpPacketLength(messageType: mt))
            if i + packetWords > wordCount { break }
            if mt == 0x2 {
                // MIDI 1.0 CV: extract the 3-byte legacy message
                // so the existing Rust decoder handles it.
                midiBuf[Int(midiCount)] = AuMidiEvent(
                    sample_offset: offset,
                    status: UInt8((w0 >> 16) & 0xFF),
                    data1: UInt8((w0 >> 8) & 0xFF),
                    data2: UInt8(w0 & 0xFF),
                    _pad: 0)
                midiCount += 1
            } else if mt == 0x4 {
                // MIDI 2.0 CV: forward two words verbatim. The
                // Rust decoder (`decode_ump_channel_voice_2`)
                // reads them as `[u32; 4]` (zero-padded).
                let w1 = (wordsPtr + Int(i) + 1).pointee
                midi2Buf[Int(midi2Count)] = AuMidi2Event(
                    sample_offset: offset,
                    words: (w0, w1, 0, 0))
                midi2Count += 1
            }
            i += packetWords
        }
        // Advance to the next packet. Variable-length packets are
        // tightly packed; total bytes = header (12) + wordCount * 4.
        pktBase = pktBase.advanced(by: 12 + Int(wordCount) * 4)
        packetIdx += 1
    }
    return (midiCount - midiStart, midi2Count - midi2Start)
}

// MARK: - AUAudioUnit subclass

class TruceAUAudioUnit: AUAudioUnit {
    private(set) var rustCtx: UnsafeMutableRawPointer?
    /// Set to true during GUI→host param sync to prevent observer feedback.
    var isSyncingToHost = false

    private var _inputBusArray: AUAudioUnitBusArray!
    private var _outputBusArray: AUAudioUnitBusArray!
    private var _parameterTree: AUParameterTree?
    private var _sampleRate: Double = 44100.0
    private var _maxFrames: UInt32 = 1024

    override init(componentDescription: AudioComponentDescription,
                  options: AudioComponentInstantiationOptions = []) throws {
        try super.init(componentDescription: componentDescription, options: options)

        guard let callbacks = g_callbacks, let descriptor = g_descriptor else { return }

        rustCtx = callbacks.pointee.create()
        logger.info("AU init: in=\(descriptor.pointee.num_inputs) out=\(descriptor.pointee.num_outputs)")
        let numIn = descriptor.pointee.num_inputs
        let numOut = descriptor.pointee.num_outputs

        if numIn > 0 {
            let inputFmt = AVAudioFormat(standardFormatWithSampleRate: 44100, channels: numIn)!
            let inBus = try AUAudioUnitBus(format: inputFmt)
            _inputBusArray = AUAudioUnitBusArray(audioUnit: self, busType: .input, busses: [inBus])
        } else {
            _inputBusArray = AUAudioUnitBusArray(audioUnit: self, busType: .input, busses: [])
        }

        // Even `aumi` (MIDI Processor, num_outputs=0) needs an
        // output bus: Apple's AUv3 framework uses it to negotiate
        // the sample rate for MIDI timing, and rejects plugins
        // with an empty output bus array via -10868
        // (kAudioUnitErr_FormatNotSupported) at instantiation.
        // The output bus exists purely so the framework can read a
        // sample rate from its format - no audio is ever written
        // to it. The render block below memsets the output buffer
        // to 0 for aumi plugins to satisfy strict hosts that audit
        // for stale data.
        // (numOut=0 in `render()` skips the output pointer setup).
        let outChans = numOut > 0 ? numOut : 2
        let outputFmt = AVAudioFormat(standardFormatWithSampleRate: 44100, channels: outChans)!
        let outBus = try AUAudioUnitBus(format: outputFmt)
        _outputBusArray = AUAudioUnitBusArray(audioUnit: self, busType: .output, busses: [outBus])

        buildParameterTree()

    }

    deinit {
        if let ctx = rustCtx, let callbacks = g_callbacks {
            callbacks.pointee.destroy(ctx)
        }
    }

    override var inputBusses: AUAudioUnitBusArray { _inputBusArray }
    override var outputBusses: AUAudioUnitBusArray { _outputBusArray }
    override var parameterTree: AUParameterTree? {
        get { _parameterTree }
        set { _parameterTree = newValue }
    }

    /// MIDI output ports exposed to the host. `aumi` (MIDI Processor)
    /// plugins - identified here by "no audio outputs declared" -
    /// must advertise at least one MIDI output for Apple's AU
    /// infrastructure to accept instantiation. Plugins with audio
    /// outputs (`aufx`, `aumu`, `aumf`) return an empty array so
    /// hosts don't surface phantom MIDI ports.
    override var midiOutputNames: [String] {
        if let d = g_descriptor?.pointee, d.num_outputs == 0 {
            return ["MIDI Out"]
        }
        return []
    }

    private func buildParameterTree() {
        // We need `rustCtx` to be non-nil (the body uses `rustCtx!`
        // below) but the value is only consumed through the closures
        // that capture it explicitly, so the `let ctx = ...` binding
        // would be unused. `case .some` matches both the
        // existence check and silences the unused-binding warning
        // without forcing a runtime !-unwrap here.
        guard let callbacks = g_callbacks, case .some = rustCtx else { return }
        var params: [AUParameter] = []
        var groups: [String: [AUParameter]] = [:]

        for i in 0..<g_num_params {
            let desc = g_param_descriptors.advanced(by: Int(i)).pointee
            let name = String(cString: desc.name)
            let group = String(cString: desc.group)
            let param = AUParameterTree.createParameter(
                withIdentifier: "param\(desc.id)", name: name,
                address: AUParameterAddress(desc.id),
                min: AUValue(desc.min), max: AUValue(desc.max),
                unit: .generic, unitName: nil,
                flags: [.flag_IsWritable, .flag_IsReadable],
                valueStrings: nil, dependentParameters: nil)
            param.value = AUValue(desc.default_value)
            if group.isEmpty { params.append(param) }
            else { groups[group, default: []].append(param) }
        }

        var children: [AUParameterNode] = params
        for (gn, gp) in groups {
            children.append(AUParameterTree.createGroup(withIdentifier: gn, name: gn, children: gp))
        }
        _parameterTree = AUParameterTree.createTree(withChildren: children)

        let rawCtx = rustCtx!
        let cb = callbacks.pointee
        _parameterTree?.implementorValueObserver = { [weak self] p, v in
            guard self?.isSyncingToHost != true else { return }
            cb.param_set_value(rawCtx, UInt32(p.address), Double(v))
        }
        _parameterTree?.implementorValueProvider = { p in
            AUValue(cb.param_get_value(rawCtx, UInt32(p.address)))
        }
        _parameterTree?.implementorStringFromValueCallback = { p, vp in
            let val = vp?.pointee ?? p.value
            var buf = [CChar](repeating: 0, count: 128)
            let len = cb.param_format_value(rawCtx, UInt32(p.address), Double(val), &buf, 128)
            return len > 0 ? String(cString: buf) : String(format: "%.2f", val)
        }
    }

    // MARK: Render

    override func allocateRenderResources() throws {
        try super.allocateRenderResources()
        if _outputBusArray.count > 0 { _sampleRate = _outputBusArray[0].format.sampleRate }
        _maxFrames = maximumFramesToRender
        if let ctx = rustCtx, let cb = g_callbacks { cb.pointee.reset(ctx, _sampleRate, _maxFrames) }
    }

    override func deallocateRenderResources() { super.deallocateRenderResources() }

    private static func render(
        ctx: UnsafeMutableRawPointer, cb: UnsafePointer<AuCallbacks>,
        numIn: UInt32, numOut: UInt32,
        timestamp: UnsafePointer<AudioTimeStamp>, frameCount: UInt32,
        outputData: UnsafeMutablePointer<AudioBufferList>,
        events: UnsafePointer<AURenderEvent>?, pull: AURenderPullInputBlock?,
        inPtrs: UnsafeMutablePointer<UnsafePointer<Float>?>,
        outPtrs: UnsafeMutablePointer<UnsafeMutablePointer<Float>?>,
        midiBuf: UnsafeMutablePointer<AuMidiEvent>,
        midi2Buf: UnsafeMutablePointer<AuMidi2Event>,
        paramBuf: UnsafeMutablePointer<AuParamEvent>,
        transportBuf: UnsafeMutablePointer<AuTransportSnapshot>,
        sysexOutScratch: UnsafeMutablePointer<UInt8>,
        sysexOutScratchCap: Int,
        musicalContext: AUHostMusicalContextBlock?,
        transportState: AUHostTransportStateBlock?,
        midiOutputBlock: AUMIDIOutputEventBlock?
    ) -> AUAudioUnitStatus {
        if numIn > 0, let pull = pull {
            var f = AudioUnitRenderActionFlags()
            let s = pull(&f, timestamp, frameCount, 0, outputData)
            if s != noErr { return s }
        }
        var numMidi: UInt32 = 0
        var numMidi2: UInt32 = 0
        var numParam: UInt32 = 0
        let bufStart = Int64(timestamp.pointee.mSampleTime)
        var ev = events
        while let event = ev, numMidi < 256 && numMidi2 < 256 && numParam < 256 {
            let head = event.pointee.head
            if head.eventType == .MIDI {
                let m = event.pointee.MIDI
                // Convert absolute eventSampleTime to relative offset within buffer
                let absTime = m.eventSampleTime
                let relOffset = max(0, absTime - bufStart)
                midiBuf[Int(numMidi)] = AuMidiEvent(
                    sample_offset: UInt32(min(relOffset, Int64(frameCount - 1))),
                    status: m.data.0, data1: m.length > 1 ? m.data.1 : 0,
                    data2: m.length > 2 ? m.data.2 : 0, _pad: 0)
                numMidi += 1
            } else if head.eventType.rawValue == kAURenderEventMIDIEventListRaw {
                // iOS 17+ / macOS 14+: AU hosts deliver UMPs through
                // AURenderEvent.MIDIEventsList (CoreMIDI's MIDIEventList
                // structure). Walk the packet list and classify each
                // word group by UMP message type - MIDI 1.0 channel
                // voice (mt=0x2, 32 bits) flows through the legacy
                // 3-byte path; MIDI 2.0 channel voice (mt=0x4, 64
                // bits) flows through midi2Buf. Other UMP types
                // (utility, system, SysEx, data) are not surfaced.
                let (midiAdded, midi2Added) = forwardMIDIEventList(
                    event: event,
                    bufStart: bufStart,
                    frameCount: frameCount,
                    midiBuf: midiBuf, midiStart: numMidi,
                    midi2Buf: midi2Buf, midi2Start: numMidi2)
                numMidi += midiAdded
                numMidi2 += midi2Added
            } else if head.eventType == .parameter || head.eventType == .parameterRamp {
                // Decode .parameter / .parameterRamp into AuParamEvent
                // with the proper within-block sample offset so the
                // Rust chunker can split the audio block at each
                // automation point. Ramp events get treated as a step
                // at the ramp's start (eventSampleTime); the plugin's
                // own smoother handles the actual interpolation. This
                // matches truce-vst3's step-at-point treatment of VST3
                // parameter queues.
                //
                // `eventSampleTime` is absolute (host timeline);
                // subtract `bufStart` to get a within-block offset
                // and clamp to [0, frameCount - 1] for safety.
                let absTime = event.pointee.parameter.eventSampleTime
                let relOffset = max(0, absTime - bufStart)
                let clamped = UInt32(min(relOffset, Int64(frameCount - 1)))
                paramBuf[Int(numParam)] = AuParamEvent(
                    sample_offset: clamped,
                    param_id: UInt32(event.pointee.parameter.parameterAddress),
                    value: event.pointee.parameter.value)
                numParam += 1
            }
            ev = UnsafePointer(head.next)
        }
        let abl = UnsafeMutableAudioBufferListPointer(outputData)
        let bufCount = abl.count
        for i in 0..<32 { inPtrs[i] = nil; outPtrs[i] = nil }
        for c in 0..<min(Int(numIn), bufCount) {
            let p: UnsafeMutablePointer<Float>? = abl[c].mData?.assumingMemoryBound(to: Float.self)
            inPtrs[c] = UnsafePointer(p)
        }
        for c in 0..<min(Int(numOut), bufCount) {
            outPtrs[c] = abl[c].mData?.assumingMemoryBound(to: Float.self)
        }

        // Fill the transport snapshot from the host-provided blocks.
        // Both are optional: hosts that don't place the plugin in a
        // musical context leave them nil.
        transportBuf.pointee = AuTransportSnapshot(
            valid: 0, playing: 0, recording: 0, loop_active: 0,
            time_sig_num: 0, time_sig_den: 0,
            tempo: 0, position_samples: 0, position_beats: 0,
            bar_start_beats: 0, loop_start_beats: 0, loop_end_beats: 0)
        if let musical = musicalContext {
            var tempo: Double = 0
            var tsigNum: Double = 0
            var tsigDen: Int = 0
            var beat: Double = 0
            var nextBeat: Int = 0
            var downbeat: Double = 0
            if musical(&tempo, &tsigNum, &tsigDen, &beat, &nextBeat, &downbeat) {
                transportBuf.pointee.tempo = tempo
                transportBuf.pointee.time_sig_num = Int32(tsigNum)
                transportBuf.pointee.time_sig_den = Int32(tsigDen)
                transportBuf.pointee.position_beats = beat
                transportBuf.pointee.bar_start_beats = downbeat
                transportBuf.pointee.valid = 1
            }
        }
        if let state = transportState {
            var flags = AUHostTransportStateFlags(rawValue: 0)
            var samplePos: Double = 0
            var cycleStart: Double = 0
            var cycleEnd: Double = 0
            if state(&flags, &samplePos, &cycleStart, &cycleEnd) {
                transportBuf.pointee.playing =
                    flags.contains(.moving) ? 1 : 0
                transportBuf.pointee.recording =
                    flags.contains(.recording) ? 1 : 0
                transportBuf.pointee.loop_active =
                    flags.contains(.cycling) ? 1 : 0
                transportBuf.pointee.position_samples = samplePos
                transportBuf.pointee.loop_start_beats = cycleStart
                transportBuf.pointee.loop_end_beats = cycleEnd
                transportBuf.pointee.valid = 1
            }
        }
        if transportBuf.pointee.valid == 0 {
            let ts = timestamp.pointee
            if (ts.mFlags.rawValue &
                AudioTimeStampFlags.sampleTimeValid.rawValue) != 0 {
                transportBuf.pointee.position_samples = ts.mSampleTime
                transportBuf.pointee.valid = 1
            }
        }

        cb.pointee.process(ctx, inPtrs, outPtrs, numIn, numOut,
                           frameCount, midiBuf, numMidi,
                           midi2Buf, numMidi2,
                           paramBuf, numParam,
                           transportBuf)

        // Drain plug-in → host MIDI output. AU v3 hosts expose a
        // `midiOutputEventBlock` that accepts a raw MIDI 1.0 byte
        // stream; we call it once per event. `eventSampleTime` is
        // the host's absolute sample time, so the plug-in's
        // within-block `delta` is added to the buffer's starting
        // sample. The block is nil when the host doesn't accept
        // MIDI output; skipping the drain is correct in that case.
        if let outputBlock = midiOutputBlock {
            let bufStart = Int64(timestamp.pointee.mSampleTime)
            // Channel-voice events. 2-byte messages (Program Change,
            // Channel Pressure) emit only the bytes that matter;
            // 3-byte messages emit all three. Buffer is stack-local
            // because the call is synchronous.
            let cvCount = cb.pointee.output_event_count(ctx)
            for i in 0..<cvCount {
                var ev = AuMidiEvent(
                    sample_offset: 0, status: 0, data1: 0, data2: 0, _pad: 0)
                cb.pointee.output_event_at(ctx, i, &ev)
                let st = ev.status & 0xF0
                let len = (st == 0xC0 || st == 0xD0) ? 2 : 3
                let bytes: [UInt8] = [ev.status, ev.data1, ev.data2]
                let evTime = AUEventSampleTime(bufStart + Int64(ev.sample_offset))
                _ = bytes.withUnsafeBufferPointer { buf in
                    outputBlock(evTime, 0 /* cable */, len, buf.baseAddress!)
                }
            }

            // SysEx events. Each event's framed payload (`0xF0` +
            // inner + `0xF7`) lands in `sysexOutScratch` so the
            // pointer the host receives stays valid for the call
            // duration. Scratch advances per event so concurrent
            // events within one block don't overwrite each other.
            let sxCount = cb.pointee.output_sysex_count(ctx)
            var scratchUsed = 0
            for i in 0..<sxCount {
                var delta: UInt32 = 0
                var bytes: UnsafePointer<UInt8>? = nil
                var len: UInt32 = 0
                cb.pointee.output_sysex_at(ctx, i, &delta, &bytes, &len)
                guard let payload = bytes else { continue }
                let framedLen = Int(len) + 2 // +0xF0 / +0xF7
                if scratchUsed + framedLen > sysexOutScratchCap { break }
                let dst = sysexOutScratch.advanced(by: scratchUsed)
                dst[0] = 0xF0
                if len > 0 {
                    dst.advanced(by: 1).update(from: payload, count: Int(len))
                }
                dst[Int(len) + 1] = 0xF7
                let evTime = AUEventSampleTime(bufStart + Int64(delta))
                _ = outputBlock(evTime, 0 /* cable */, framedLen, UnsafePointer(dst))
                scratchUsed += framedLen
            }
        }
        return noErr
    }

    override var internalRenderBlock: AUInternalRenderBlock {
        let ctx = rustCtx!
        let cb = g_callbacks!
        let numIn = g_descriptor?.pointee.num_inputs ?? 0
        let numOut = g_descriptor?.pointee.num_outputs ?? 2
        let inPtrs = UnsafeMutablePointer<UnsafePointer<Float>?>.allocate(capacity: 32)
        let outPtrs = UnsafeMutablePointer<UnsafeMutablePointer<Float>?>.allocate(capacity: 32)
        let midiBuf = UnsafeMutablePointer<AuMidiEvent>.allocate(capacity: 256)
        let midi2Buf = UnsafeMutablePointer<AuMidi2Event>.allocate(capacity: 256)
        // Per-block scratch for host-side parameter automation
        // events. AURenderEvent's `.parameter` / `.parameterRamp`
        // entries land here with a within-block `sample_offset` so
        // the Rust chunker splits the audio block at each
        // automation point. 256 slots matches the MIDI scratches;
        // typical hosts emit at most a handful per block.
        let paramBuf = UnsafeMutablePointer<AuParamEvent>.allocate(capacity: 256)
        let transportBuf = UnsafeMutablePointer<AuTransportSnapshot>.allocate(capacity: 1)
        // Scratch for output SysEx framing: each plug-in event
        // becomes `0xF0` + inner + `0xF7` in this buffer before
        // being handed to `midiOutputEventBlock`. Sized to
        // `TRUCE_SYSEX_POOL_PREALLOC` (mirrored from
        // `truce_core::SYSEX_POOL_PREALLOC`, 128 KiB by default -
        // the worst-case sum of all inner payloads in one block)
        // plus 512 B of framing headroom (2 bytes × up to 256
        // events).
        let sysexOutScratchCap = Int(TRUCE_SYSEX_POOL_PREALLOC) + 512
        let sysexOutScratch =
            UnsafeMutablePointer<UInt8>.allocate(capacity: sysexOutScratchCap)

        // Snapshot the host blocks at render-graph compile time. AU v3
        // guarantees these are realtime-safe to call from the render
        // block; hosts may set them post-initialization, so this copy
        // will be nil for plugins instantiated outside a musical context.
        let musicalContext = self.musicalContextBlock
        let transportState = self.transportStateBlock
        let midiOutputBlock = self.midiOutputEventBlock

        return { _, timestamp, frameCount, _, outputData, events, pull in
            return TruceAUAudioUnit.render(
                ctx: ctx, cb: cb, numIn: numIn, numOut: numOut,
                timestamp: timestamp, frameCount: frameCount,
                outputData: outputData, events: events, pull: pull,
                inPtrs: inPtrs, outPtrs: outPtrs, midiBuf: midiBuf,
                midi2Buf: midi2Buf,
                paramBuf: paramBuf,
                transportBuf: transportBuf,
                sysexOutScratch: sysexOutScratch,
                sysexOutScratchCap: sysexOutScratchCap,
                musicalContext: musicalContext,
                transportState: transportState,
                midiOutputBlock: midiOutputBlock)
        }
    }

    // MARK: State

    override var fullState: [String: Any]? {
        get {
            var state = super.fullState ?? [:]
            guard let ctx = rustCtx, let cb = g_callbacks else { return state }
            var data: UnsafeMutablePointer<UInt8>? = nil
            var len: UInt32 = 0
            cb.pointee.state_save(ctx, &data, &len)
            if let data = data, len > 0 {
                state["truce_state"] = Data(bytes: data, count: Int(len))
                cb.pointee.state_free(data, len)
            }
            return state
        }
        set {
            super.fullState = newValue
            guard let blob = newValue?["truce_state"] as? Data,
                  let ctx = rustCtx, let cb = g_callbacks else { return }
            blob.withUnsafeBytes { ptr in
                cb.pointee.state_load(ctx, ptr.baseAddress?.assumingMemoryBound(to: UInt8.self), UInt32(blob.count))
            }
            if let tree = _parameterTree {
                for param in tree.allParameters {
                    param.value = AUValue(g_callbacks!.pointee.param_get_value(ctx, UInt32(param.address)))
                }
            }
        }
    }

    // MARK: Capabilities

    override var channelCapabilities: [NSNumber]? {
        guard let d = g_descriptor?.pointee else { return nil }
        // aumi (MIDI Processor, zero audio I/O): advertise 0 inputs
        // and "any" (-1) outputs - the output bus is a dummy kept
        // alive only so AUv3 can negotiate a sample rate. This
        // `[0, -1]` shape is the AUChannelInfo Apple's framework
        // requires for AU MIDI effects.
        if d.num_inputs == 0 && d.num_outputs == 0 {
            return [0, -1]
        }
        if d.num_inputs == 0 {
            return [0, NSNumber(value: d.num_outputs)]
        }
        return [NSNumber(value: d.num_inputs), NSNumber(value: d.num_outputs)]
    }
    override var isMusicDeviceOrEffect: Bool { true }
    override var canProcessInPlace: Bool { g_descriptor?.pointee.num_inputs ?? 0 > 0 }
    override var latency: TimeInterval { 0 }
    override var tailTime: TimeInterval { 0 }
    override var shouldBypassEffect: Bool { get { false } set { } }
}

// MARK: - Factory

// `@objc(AudioUnitFactory)` pins the runtime class name to
// `AudioUnitFactory` (no module prefix) and - critically - forces
// Swift's optimizer to keep the class in `__objc_classlist`.
// Without this, `swiftc -O` strips the class because nothing in
// the module references it directly; NSExtension's runtime lookup
// (via `NSExtensionPrincipalClass`) is invisible to the optimizer
// and the appex launches with no principal class, dying with
// XPC error 4097 (`NSXPCConnectionInvalid`). The matching
// Info.plist key is `<key>NSExtensionPrincipalClass</key>
// <string>AudioUnitFactory</string>` (no `$(PRODUCT_MODULE_NAME).`
// prefix).
@objc(AudioUnitFactory)
class AudioUnitFactory: AUViewController, AUAudioUnitFactory {
    private var auInstance: TruceAUAudioUnit?

    public func createAudioUnit(with componentDescription: AudioComponentDescription) throws -> AUAudioUnit {
        let au = try TruceAUAudioUnit(componentDescription: componentDescription, options: [])
        auInstance = au
        logger.info("factory createAudioUnit")
        // If the view is already loaded (host called loadView before
        // createAudioUnit), set up the GUI now that we have an instance.
        // Must dispatch to main thread - NSView operations require it.
        if isViewLoaded {
            DispatchQueue.main.async { [weak self] in
                self?.setupGUIIfReady()
            }
        }
        return au
    }

    private var guiSetUp = false
    private var guiContainer: NSView?
    private var guiPtSize: NSSize = .zero
    private var paramSyncTimer: Timer?

    override func loadView() {
        // Query editor size using a temporary Rust context.
        var size = NSSize(width: 200, height: 150) // fallback
        if let cb = g_callbacks {
            let tmpCtx = cb.pointee.create()
            if let ctx = tmpCtx {
                if cb.pointee.gui_has_editor(ctx) != 0 {
                    var w: UInt32 = 0, h: UInt32 = 0
                    cb.pointee.gui_get_size(ctx, &w, &h)
                    if w > 0 && h > 0 {
                        // w/h are in logical points - use directly.
                        size = NSSize(width: CGFloat(w), height: CGFloat(h))
                    }
                }
                cb.pointee.destroy(ctx)
            }
        }
        let v = NSView(frame: NSRect(origin: .zero, size: size))
        #if os(macOS)
        v.wantsLayer = true
        v.layer?.backgroundColor = CGColor(red: 0.15, green: 0.15, blue: 0.15, alpha: 1)
        #else
        // UIView always has a backing layer; set the BG directly.
        v.backgroundColor = UIColor(red: 0.15, green: 0.15, blue: 0.15, alpha: 1)
        #endif
        self.view = v
        self.preferredContentSize = size
        logger.info("loadView: \(size.width)x\(size.height)")
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        logger.info("viewDidLoad: view.frame=\(self.view.frame.width)x\(self.view.frame.height) auInstance=\(self.auInstance != nil)")
        setupGUIIfReady()
    }

    #if os(macOS)
    override func viewWillAppear() {
        super.viewWillAppear()
        logger.info("viewWillAppear: view.frame=\(self.view.frame.width)x\(self.view.frame.height)")
        setupGUIIfReady()
    }
    #else
    override func viewWillAppear(_ animated: Bool) {
        super.viewWillAppear(animated)
        logger.info("viewWillAppear: view.frame=\(self.view.frame.width)x\(self.view.frame.height)")
        setupGUIIfReady()
    }
    #endif


    /// Rust context for THIS instance's AU (per-instance).
    private var myCtx: UnsafeMutableRawPointer? {
        auInstance?.rustCtx
    }

    private func setupGUIIfReady() {
        logger.info("setupGUIIfReady: guiSetUp=\(self.guiSetUp) auInstance=\(self.auInstance != nil)")
        guard !guiSetUp,
              let ctx = myCtx,
              let cb = g_callbacks,
              cb.pointee.gui_has_editor(ctx) != 0 else { return }

        var w: UInt32 = 0
        var h: UInt32 = 0
        cb.pointee.gui_get_size(ctx, &w, &h)
        guard w > 0, h > 0 else { return }

        // w/h are in logical points - use directly.
        guiPtSize = NSSize(width: CGFloat(w), height: CGFloat(h))
        logger.info("setupGUI: \(w)x\(h) view=\(self.view.frame.width)x\(self.view.frame.height)")

        let container = NSView(frame: NSRect(origin: .zero, size: guiPtSize))
        cb.pointee.gui_open(ctx, Unmanaged.passUnretained(container).toOpaque())

        for sub in self.view.subviews { sub.removeFromSuperview() }
        self.view.addSubview(container)
        guiContainer = container
        self.preferredContentSize = guiPtSize
        // Center the GUI in the host's view (which may be oversized)
        centerGUI()
        guiSetUp = true

        // Sync Rust param values → AUParameterTree at ~30fps.
        // This ensures the host sees GUI-initiated param changes (KVO).
        startParamSync()
    }

    #if os(macOS)
    override func viewDidDisappear() {
        super.viewDidDisappear()
        teardownGUI()
    }

    override func viewDidLayout() {
        super.viewDidLayout()
        centerGUI()
    }
    #else
    override func viewDidDisappear(_ animated: Bool) {
        super.viewDidDisappear(animated)
        teardownGUI()
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        propagateHostResize()
        centerGUI()
    }
    #endif

    /// When the host's container view changes our bounds (drag-
    /// resize), forward to `gui_set_size` so the editor follows.
    /// No-op when the editor opted out of resize - in that case
    /// `centerGUI` keeps the inner container at its original size.
    private func propagateHostResize() {
        guard guiSetUp,
              let ctx = myCtx,
              let cb = g_callbacks,
              cb.pointee.gui_can_resize(ctx) != 0
        else { return }
        let hostW = self.view.bounds.width
        let hostH = self.view.bounds.height
        guard hostW > 0, hostH > 0,
              (hostW, hostH) != (guiPtSize.width, guiPtSize.height)
        else { return }
        let newW = UInt32(max(1, hostW.rounded()))
        let newH = UInt32(max(1, hostH.rounded()))
        cb.pointee.gui_set_size(ctx, newW, newH)
        guiPtSize = NSSize(width: CGFloat(newW), height: CGFloat(newH))
        // Update the inner container's frame so the editor's NSView
        // has the right outer bounds when its `on_frame` picks up
        // the pending-size cell. `centerGUI` below repositions to
        // origin (0, 0) since the new size matches the host's.
        guiContainer?.frame = NSRect(origin: .zero, size: guiPtSize)
    }

    private func teardownGUI() {
        // Close the GUI when the host hides the plugin window.
        // This stops the repaint timer. setupGUIIfReady will
        // re-create the GUI when the window is shown again.
        stopParamSync()
        if guiSetUp, let ctx = myCtx, let cb = g_callbacks {
            cb.pointee.gui_close(ctx)
        }
        guiContainer?.removeFromSuperview()
        guiContainer = nil
        guiSetUp = false
    }

    private func centerGUI() {
        guard let container = guiContainer, guiPtSize.width > 0 else { return }
        let hostW = self.view.bounds.width
        let hostH = self.view.bounds.height
        let x = max(0, (hostW - guiPtSize.width) / 2)
        let y = max(0, (hostH - guiPtSize.height) / 2)
        container.frame = NSRect(x: x, y: y,
                                 width: guiPtSize.width, height: guiPtSize.height)
    }

    // MARK: - Parameter sync (GUI ↔ host)

    private func startParamSync() {
        paramSyncTimer = Timer.scheduledTimer(withTimeInterval: 1.0/30.0, repeats: true) { [weak self] _ in
            self?.syncParamsToHost()
        }
    }

    private func stopParamSync() {
        paramSyncTimer?.invalidate()
        paramSyncTimer = nil
    }

    /// Push current Rust param values to the AUParameterTree so the host
    /// sees changes made via the custom GUI (triggers KVO notification).
    private var syncCount = 0

    private func syncParamsToHost() {
        guard let au = auInstance,
              let ctx = au.rustCtx,
              let cb = g_callbacks,
              let tree = au.parameterTree else { return }

        au.isSyncingToHost = true
        for param in tree.allParameters {
            let rustVal = AUValue(cb.pointee.param_get_value(ctx, UInt32(param.address)))
            if abs(param.value - rustVal) > 1e-4 {
                param.setValue(rustVal, originator: nil)
            }
        }
        au.isSyncingToHost = false
    }
}
