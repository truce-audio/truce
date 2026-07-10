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

/// The ABI tail version the plugin binary declares, or 0 when the
/// version word lacks its 'TAu\0' magic tag. A pre-2.0 binary has the
/// `create` function pointer at offset 0; without the magic check its
/// low bits would masquerade as a version and every gated tail read
/// would land one slot off. The renamed `truce_au_register_v2` only
/// protects the staticlib/shim link inside the framework; this appex
/// binds the framework through `g_callbacks` / `g_descriptor` /
/// `truce_au_init`, whose names predate 2.0, so version skew across
/// that boundary is caught only by this runtime check.
func truceAbiTailVersion(_ cb: UnsafePointer<AuCallbacks>) -> UInt32 {
    let word = cb.pointee.abi_version
    guard word & TRUCE_AU_ABI_MAGIC_MASK == TRUCE_AU_ABI_MAGIC else { return 0 }
    return word & 0xFF
}

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
    // AURenderEvent is a tagged union; reach into the variable-length
    // `MIDIEventsList` tail via raw pointer arithmetic so the code
    // compiles on Xcodes that haven't imported the overlay symbol.
    // Only the tail needs hand offsets - the fixed header (timing) is
    // read through the Swift overlay to avoid mis-offsetting it:
    //   AUMIDIEventList { AURenderEventHeader head; MIDIEventList list; }
    // `eventSampleTime` lives inside `head`; the list begins one
    // aligned `AURenderEventHeader` past the event base. Reading the
    // timestamp from a hand offset instead traps on `Int64` overflow
    // when the offset lands on header padding.
    let raw = UnsafeRawPointer(event)
    let absTime = event.pointee.head.eventSampleTime
    let relOffset = max(0, absTime - bufStart)
    let offset = UInt32(min(relOffset, Int64(frameCount - 1)))
    // MIDIEventList layout (CoreMIDI/MIDIServices.h):
    //   MIDIProtocolID protocol; uint32_t numPackets;
    //   MIDIEventPacket packet[1];  // variable-length tail
    let listBase = raw.advanced(by: MemoryLayout<AURenderEventHeader>.stride)
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
                    port: 0)
                midiCount += 1
            } else if mt == 0x3 || mt == 0x4 || mt == 0x5 {
                // MIDI 2.0 CV (0x4), SysEx-7 (0x3), SysEx-8 (0x5):
                // forward the packet words verbatim. The Rust side
                // dispatches on message type - decoding CV via
                // `decode_ump_channel_voice_2` and reassembling the
                // SysEx-7/8 packet chains into one `EventBody::SysEx`.
                // Reads as `[u32; 4]`; zero-pad the unused tail.
                let w1 = packetWords > 1 ? (wordsPtr + Int(i) + 1).pointee : 0
                let w2 = packetWords > 2 ? (wordsPtr + Int(i) + 2).pointee : 0
                let w3 = packetWords > 3 ? (wordsPtr + Int(i) + 3).pointee : 0
                midi2Buf[Int(midi2Count)] = AuMidi2Event(
                    sample_offset: offset,
                    words: (w0, w1, w2, w3))
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

// A deinterleaved float32 format for `channels`. `standardFormat` only
// defines mono/stereo layouts and returns nil for wider counts, so a
// surround bus (declared via a multi-entry `bus_layouts()`) needs an
// explicit channel layout or the force-unwrap would trap at init and the
// host would see the appex fail to open (OpenAComponent 4097).
func truceAudioFormat(sampleRate: Double, channels: AVAudioChannelCount) -> AVAudioFormat? {
    if channels <= 2 {
        return AVAudioFormat(standardFormatWithSampleRate: sampleRate, channels: channels)
    }
    let tag: AudioChannelLayoutTag
    switch channels {
    case 3: tag = kAudioChannelLayoutTag_MPEG_3_0_A
    case 4: tag = kAudioChannelLayoutTag_Quadraphonic
    case 5: tag = kAudioChannelLayoutTag_MPEG_5_0_A
    case 6: tag = kAudioChannelLayoutTag_MPEG_5_1_A
    case 7: tag = kAudioChannelLayoutTag_MPEG_6_1_A
    case 8: tag = kAudioChannelLayoutTag_MPEG_7_1_A
    default: return nil
    }
    guard let layout = AVAudioChannelLayout(layoutTag: tag) else { return nil }
    return AVAudioFormat(standardFormatWithSampleRate: sampleRate, channelLayout: layout)
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
    /// Last latency (samples) pushed to the host via KVO. The framework
    /// refreshes its latency cache each block; a main-thread timer
    /// compares against this and fires KVO on `latency` when it moves, so
    /// a plugin that varies its latency reaches the host.
    private var _lastLatencySamples: UInt32 = 0
    /// Polls `latency` while render resources are allocated, so a latency
    /// change driven by host automation (no editor open) still notifies.
    /// Slow (a few Hz) - latency moves on mode switches, not per block.
    private var _latencyTimer: Timer?

    override init(componentDescription: AudioComponentDescription,
                  options: AudioComponentInstantiationOptions = []) throws {
        try super.init(componentDescription: componentDescription, options: options)

        guard let callbacks = g_callbacks, let descriptor = g_descriptor else { return }

        // A pre-2.0 framework loads cleanly (the globals above kept
        // their names) but lays out `AuCallbacks` differently -
        // calling `create` there would jump through the wrong slot.
        // Refuse instantiation instead of crashing the extension.
        guard truceAbiTailVersion(callbacks) >= 1 else {
            logger.error("AU init: plugin binary predates the AU ABI version word; rebuild the framework with a matching truce version")
            throw NSError(domain: NSOSStatusErrorDomain,
                          code: Int(kAudioUnitErr_FailedInitialization))
        }

        rustCtx = callbacks.pointee.create()
        logger.info("AU init: in=\(descriptor.pointee.num_inputs) out=\(descriptor.pointee.num_outputs)")
        let numIn = descriptor.pointee.num_inputs
        let numOut = descriptor.pointee.num_outputs

        if numIn > 0 {
            guard let inputFmt = truceAudioFormat(sampleRate: 44100, channels: numIn) else {
                throw NSError(domain: NSOSStatusErrorDomain,
                              code: Int(kAudioUnitErr_FormatNotSupported))
            }
            // Bus 0 is the main input. A declared sidechain adds bus 1 so
            // the host can route a separate source to it; the render block
            // pulls it and concatenates its channels after the main ones.
            let mainBus = try AUAudioUnitBus(format: inputFmt)
            mainBus.name = "Input"
            var inBusses = [mainBus]
            let scChans = descriptor.pointee.sidechain_in_channels
            if scChans > 0 {
                guard let scFmt = truceAudioFormat(sampleRate: 44100, channels: scChans) else {
                    throw NSError(domain: NSOSStatusErrorDomain,
                                  code: Int(kAudioUnitErr_FormatNotSupported))
                }
                let scBus = try AUAudioUnitBus(format: scFmt)
                scBus.name = "Sidechain"
                inBusses.append(scBus)
            }
            _inputBusArray = AUAudioUnitBusArray(audioUnit: self, busType: .input, busses: inBusses)
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
        guard let outputFmt = truceAudioFormat(sampleRate: 44100, channels: outChans) else {
            throw NSError(domain: NSOSStatusErrorDomain,
                          code: Int(kAudioUnitErr_FormatNotSupported))
        }
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

    // MARK: - AU v3 view resizing

    /// Accept every view configuration the host proposes. This is what
    /// surfaces the resize / expand affordance in hosts like GarageBand -
    /// without it the host treats the AU v3 view as a single fixed size and
    /// never offers to enlarge it, regardless of the `resizable`
    /// AudioComponents tag. Returning all indices says "we can render at any
    /// size the host offers"; the embedded editor reflows to the host bounds
    /// in the view controller's layout pass (`fitGUIToSafeArea`).
    override func supportedViewConfigurations(
        _ availableViewConfigurations: [AUAudioUnitViewConfiguration]
    ) -> IndexSet {
        IndexSet(integersIn: availableViewConfigurations.indices)
    }

    /// Host picked one of the configurations reported above. The hosted view
    /// tracks its parent's bounds and refits on the next layout pass, so this
    /// only needs to exist for the host's `select` call to succeed.
    override func select(_ viewConfiguration: AUAudioUnitViewConfiguration) {}

    override var parameterTree: AUParameterTree? {
        get { _parameterTree }
        set { _parameterTree = newValue }
    }

    /// MIDI output ports exposed to the host, gated on the plugin's
    /// `emits_midi` capability (`has_midi_output` in the descriptor;
    /// note-effect default, overridable via `midi_output` in
    /// truce.toml). `aumi` MIDI Processors must advertise one for
    /// Apple's AU infrastructure to accept instantiation; an
    /// instrument or effect that opts in gets one too, and a plugin
    /// that emits no MIDI returns an empty array so hosts don't
    /// surface a phantom port.
    override var midiOutputNames: [String] {
        guard let d = g_descriptor?.pointee, d.has_midi_output != 0 else { return [] }
        // One named output per declared MIDI output port. The plugin
        // routes each event to a port via `Event::port`, which the
        // render drain passes as the `cable` to `midiOutputEventBlock`.
        // Numbered only when there's more than one, so the common
        // single-port case keeps the plain "MIDI Out" label.
        let n = max(1, Int(d.midi_output_ports))
        if n == 1 { return ["MIDI Out"] }
        return (1...n).map { "MIDI Out \($0)" }
    }

    /// MIDI protocol the host delivers *input* in. Declaring 2.0 makes the
    /// host send native UMP MIDI 2.0 (NoteOn2 / PerNoteCC / ...) through the
    /// render-event MIDI list, which the Rust side decodes; declaring 1.0
    /// makes the host down-convert first. Gated on `midi2_input` only - a
    /// 1.0->2.0 promoter (`midi2_output` without `midi2_input`) wants 1.0
    /// input it can read, and emits 2.0 on its own *output* stream, which is
    /// negotiated separately (see the output drain below). A plugin that
    /// didn't ask for MIDI 2.0 input never sees the 2.0 variants - the same
    /// contract as CLAP (which only advertises `CLAP_NOTE_DIALECT_MIDI2`
    /// when opted in). Without this override the default is 1.0, so the Rust
    /// 2.0 decode path would stay dormant.
    @available(macOS 12.0, iOS 15.0, *)
    override var audioUnitMIDIProtocol: MIDIProtocolID {
        if let d = g_descriptor?.pointee, d.midi2_input != 0 {
            return ._2_0
        }
        return ._1_0
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
        if let ctx = rustCtx, let cb = g_callbacks {
            // Forward the host's offline-render flag before prep so the
            // plugin's reset / process observe the right ProcessMode
            // (0 realtime, 2 offline). `set_render_mode` is an ABI v4
            // tail callback - gate so an older plugin binary is never
            // called through a pointer past its tail.
            if truceAbiTailVersion(cb) >= 4 {
                let mode: UInt32 = isRenderingOffline ? 2 : 0
                cb.pointee.set_render_mode(ctx, mode)
            }
            cb.pointee.reset(ctx, _sampleRate, _maxFrames)
        }
        // Watch for dynamic-latency changes on the main thread while
        // rendering (KVO must fire off the audio thread). Scheduled on
        // the main run loop even if the host allocates on another thread.
        DispatchQueue.main.async { [weak self] in
            guard let self = self else { return }
            self._latencyTimer?.invalidate()
            self._latencyTimer = Timer.scheduledTimer(withTimeInterval: 0.2, repeats: true) {
                [weak self] _ in
                self?.notifyLatencyIfChanged()
            }
        }
    }

    override func deallocateRenderResources() {
        DispatchQueue.main.async { [weak self] in
            self?._latencyTimer?.invalidate()
            self?._latencyTimer = nil
        }
        super.deallocateRenderResources()
    }

    private static func render(
        ctx: UnsafeMutableRawPointer, cb: UnsafePointer<AuCallbacks>,
        numIn: UInt32, numOut: UInt32,
        timestamp: UnsafePointer<AudioTimeStamp>, frameCount: UInt32,
        outputData: UnsafeMutablePointer<AudioBufferList>,
        events: UnsafePointer<AURenderEvent>?, pull: AURenderPullInputBlock?,
        inPtrs: UnsafeMutablePointer<UnsafePointer<Float>?>,
        outPtrs: UnsafeMutablePointer<UnsafeMutablePointer<Float>?>,
        midiBuf: UnsafeMutablePointer<AuMidiEvent>,
        scCh: Int,
        scScratch: UnsafeMutablePointer<Float>?,
        scABL: UnsafeMutableAudioBufferListPointer?,
        scMaxFrames: Int,
        midi2Buf: UnsafeMutablePointer<AuMidi2Event>,
        paramBuf: UnsafeMutablePointer<AuParamEvent>,
        transportBuf: UnsafeMutablePointer<AuTransportSnapshot>,
        sysexOutScratch: UnsafeMutablePointer<UInt8>,
        sysexOutScratchCap: Int,
        musicalContext: AUHostMusicalContextBlock?,
        transportState: AUHostTransportStateBlock?,
        midiOutputBlock: AUMIDIOutputEventBlock?,
        // Type-erased `AUMIDIEventListBlock?` (the UMP output block).
        // Passed as `Any?` so this signature doesn't reference the
        // macOS-12 / iOS-15-only type; cast back under `#available`.
        midiOutputListBlock: Any?,
        // `true` when the host set `hostMIDIProtocol` to MIDI 1.0 -
        // the UMP drain then declares a 1.0 list and asks the Rust
        // side for an MT 0x2 stream.
        hostWantsMidi1: Bool
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
                    data2: m.length > 2 ? m.data.2 : 0, port: 0)
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
        // The host may run a multi-layout plugin at a narrower width than
        // its first declared layout (the descriptor's numIn / numOut), so
        // the negotiated bus - reflected in the buffer count - is the
        // authority. Clamp to it and hand the plugin the real widths, not
        // the descriptor's, so it never sees nil channel pointers.
        let actualIn = numIn > 0 ? UInt32(min(Int(numIn), bufCount)) : 0
        let actualOut = UInt32(min(Int(numOut), bufCount))
        for i in 0..<32 { inPtrs[i] = nil; outPtrs[i] = nil }
        for c in 0..<Int(actualIn) {
            let p: UnsafeMutablePointer<Float>? = abl[c].mData?.assumingMemoryBound(to: Float.self)
            inPtrs[c] = UnsafePointer(p)
        }
        for c in 0..<Int(actualOut) {
            outPtrs[c] = abl[c].mData?.assumingMemoryBound(to: Float.self)
        }

        // Pull the sidechain input (bus 1) into scratch and append its
        // channels after the main input, so the flat array is
        // [main..., sidechain...]. An unconnected sidechain reads silence.
        var scActual = 0
        if scCh > 0, let pull = pull, let scScratch = scScratch, let scABL = scABL {
            for c in 0..<scCh {
                scABL[c] = AudioBuffer(
                    mNumberChannels: 1,
                    mDataByteSize: frameCount * UInt32(MemoryLayout<Float>.size),
                    mData: UnsafeMutableRawPointer(scScratch.advanced(by: c * scMaxFrames)))
            }
            var f = AudioUnitRenderActionFlags()
            let s = pull(&f, timestamp, frameCount, 1, scABL.unsafeMutablePointer)
            if s != noErr {
                for c in 0..<scCh {
                    memset(scScratch.advanced(by: c * scMaxFrames), 0,
                           Int(frameCount) * MemoryLayout<Float>.size)
                }
            }
            let n = min(scCh, max(0, 32 - Int(actualIn)))
            for c in 0..<n {
                inPtrs[Int(actualIn) + c] =
                    UnsafePointer(scABL[c].mData?.assumingMemoryBound(to: Float.self))
            }
            scActual = n
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

        cb.pointee.process(ctx, inPtrs, outPtrs, actualIn + UInt32(scActual), actualOut,
                           frameCount, midiBuf, numMidi,
                           midi2Buf, numMidi2,
                           paramBuf, numParam,
                           transportBuf)

        // Drain plug-in → host MIDI output. `eventSampleTime` is the
        // host's absolute sample time, so the plug-in's within-block
        // `delta` is added to the buffer's starting sample. Both
        // blocks nil means the host doesn't accept MIDI output;
        // skipping the drain is correct then.
        // `bufStart` is already bound above (input timing); reuse it.

        // Preferred path: the UMP `MIDIEventList` block (macOS 12+ /
        // iOS 15+), used for every output dialect so a host that only
        // supplies the list block hears MIDI 1.0 plugins too. The Rust
        // side encodes a *pure* stream in the chosen protocol - all MT
        // 0x2 channel voice for 1.0, all MT 0x4 for 2.0, SysEx as MT 0x3
        // SysEx-7 chains (legal in both) - because the UMP spec forbids
        // mixing the two channel-voice types in one protocol stream.
        //
        // A `midi2_output` plugin's stream stays 2.0 even when the host's
        // *input* protocol (`hostMIDIProtocol`) is 1.0: its per-note UMP
        // (PerNotePitchBend / PerNoteCC) has no 1.0 form, so a 1.0 stream
        // would fold shared-channel per-note messages onto one channel.
        // Input and output are separate self-describing MIDIEventList
        // streams, so 1.0-in / 2.0-out is spec-clean per stream. A
        // 1.0-output plugin follows the host protocol.
        // The protocol-taking `output_ump_*` shape is AU ABI version
        // 2; this appex may be newer than the plugin binary, so gate
        // on the (magic-validated) reported version before draining
        // through them.
        var drainedViaUMP = false
        if truceAbiTailVersion(cb) >= 2, #available(macOS 12.0, iOS 15.0, *),
           let listBlock = midiOutputListBlock as? AUMIDIEventListBlock {
            drainedViaUMP = true
            // Force 2.0 for a `midi2_output` plugin; otherwise follow the
            // host's protocol (up-converting a 1.0 plugin to a 2.0 host so
            // the stream stays pure).
            let wantMidi2Out = (g_descriptor?.pointee.midi2_output ?? 0) != 0 || !hostWantsMidi1
            let proto: UInt32 = wantMidi2Out ? 2 : 1
            let listProto: MIDIProtocolID = wantMidi2Out ? ._2_0 : ._1_0
            let umpCount = cb.pointee.output_ump_count(ctx, proto)
            for i in 0..<umpCount {
                var ue = AuUmpEvent()
                cb.pointee.output_ump_at(ctx, proto, i, &ue)
                let evTime = AUEventSampleTime(bufStart + Int64(ue.sample_offset))
                var list = MIDIEventList()
                let pkt = MIDIEventListInit(&list, listProto)
                // Stack-borrow the C words array; a Swift Array here
                // would heap-allocate per packet inside the render
                // block, and a large SysEx is thousands of packets.
                _ = withUnsafeBytes(of: ue.words) { raw in
                    MIDIEventListAdd(&list, MemoryLayout<MIDIEventList>.size, pkt, 0,
                                     Int(ue.word_count),
                                     raw.baseAddress!.assumingMemoryBound(to: UInt32.self))
                }
                _ = listBlock(evTime, ue.cable, &list)
            }
        } else if let outputBlock = midiOutputBlock {
            // MIDI 1.0 byte path. 2-byte messages (Program Change,
            // Channel Pressure) emit only the bytes that matter; 3-byte
            // messages emit all three. `ev.port` is the plugin's chosen
            // output cable, clamped on the Rust side (AU v2 reports 0).
            let cvCount = cb.pointee.output_event_count(ctx)
            for i in 0..<cvCount {
                var ev = AuMidiEvent(
                    sample_offset: 0, status: 0, data1: 0, data2: 0, port: 0)
                cb.pointee.output_event_at(ctx, i, &ev)
                let st = ev.status & 0xF0
                let len = (st == 0xC0 || st == 0xD0) ? 2 : 3
                let bytes: [UInt8] = [ev.status, ev.data1, ev.data2]
                let evTime = AUEventSampleTime(bufStart + Int64(ev.sample_offset))
                _ = bytes.withUnsafeBufferPointer { buf in
                    outputBlock(evTime, ev.port, len, buf.baseAddress!)
                }
            }
        }

        // SysEx output on the byte path. Skipped when the UMP drain ran
        // above - there SysEx already went out as SysEx-7 packets inside
        // the `output_ump_*` stream, and re-sending it here would
        // double-deliver on hosts that supply both blocks. Each event's
        // framed payload (`0xF0` + inner + `0xF7`) lands in
        // `sysexOutScratch` so the pointer stays valid for the
        // synchronous call; scratch advances per event so concurrent
        // events don't overwrite each other.
        if !drainedViaUMP, let outputBlock = midiOutputBlock {
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
                // SysEx always goes out on cable 0 - `output_sysex_at`
                // doesn't carry the port yet (multi-port SysEx output is
                // a follow-up; channel-voice above routes by cable).
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

        // Sidechain input (bus 1) staging. The main input is pulled in
        // place into the output ABL, so the sidechain needs its own
        // scratch to pull into and append after the main channels. Sized
        // once to the render-graph's max frame count.
        let scCh = Int(g_descriptor?.pointee.sidechain_in_channels ?? 0)
        let scMaxFrames = max(Int(self.maximumFramesToRender), 4096)
        let scScratch: UnsafeMutablePointer<Float>? =
            scCh > 0 ? UnsafeMutablePointer<Float>.allocate(capacity: scCh * scMaxFrames) : nil
        let scABL: UnsafeMutableAudioBufferListPointer? =
            scCh > 0 ? AudioBufferList.allocate(maximumBuffers: scCh) : nil

        // Snapshot the host blocks at render-graph compile time. AU v3
        // guarantees these are realtime-safe to call from the render
        // block; hosts may set them post-initialization, so this copy
        // will be nil for plugins instantiated outside a musical context.
        let musicalContext = self.musicalContextBlock
        let transportState = self.transportStateBlock
        let midiOutputBlock = self.midiOutputEventBlock
        // The UMP output block (MIDI 2.0), captured type-erased so the
        // deployment target can stay below macOS 12 / iOS 15. `render`
        // casts it back under an availability check.
        let midiOutputListBlock: Any?
        if #available(macOS 12.0, iOS 15.0, *) {
            midiOutputListBlock = self.midiOutputEventListBlock
        } else {
            midiOutputListBlock = nil
        }
        // Snapshot the host's declared output protocol with the
        // blocks - hosts set `hostMIDIProtocol` before requesting the
        // render block. Unset (raw 0) defaults to 2.0, the protocol
        // CoreMIDI translates natively.
        let hostWantsMidi1: Bool
        if #available(macOS 12.0, iOS 15.0, *) {
            hostWantsMidi1 = self.hostMIDIProtocol == ._1_0
        } else {
            hostWantsMidi1 = false
        }

        return { _, timestamp, frameCount, _, outputData, events, pull in
            return TruceAUAudioUnit.render(
                ctx: ctx, cb: cb, numIn: numIn, numOut: numOut,
                timestamp: timestamp, frameCount: frameCount,
                outputData: outputData, events: events, pull: pull,
                inPtrs: inPtrs, outPtrs: outPtrs, midiBuf: midiBuf,
                scCh: scCh, scScratch: scScratch, scABL: scABL, scMaxFrames: scMaxFrames,
                midi2Buf: midi2Buf,
                paramBuf: paramBuf,
                transportBuf: transportBuf,
                sysexOutScratch: sysexOutScratch,
                sysexOutScratchCap: sysexOutScratchCap,
                musicalContext: musicalContext,
                transportState: transportState,
                midiOutputBlock: midiOutputBlock,
                midiOutputListBlock: midiOutputListBlock,
                hostWantsMidi1: hostWantsMidi1)
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
            guard let ctx = rustCtx, let cb = g_callbacks else { return }
            if let blob = newValue?["truce_state"] as? Data {
                blob.withUnsafeBytes { ptr in
                    cb.pointee.state_load(ctx, ptr.baseAddress?.assumingMemoryBound(to: UInt8.self), UInt32(blob.count))
                }
                syncParameterTreeFromRust()
                return
            }
            // No truce entry: a pre-truce build stored its state under
            // its own dictionary key. Probe the keys declared in
            // truce.toml's [plugin.legacy_state] (first present +
            // accepted wins) so the plugin's migrate_state hook can
            // translate the old session. These callbacks live at the
            // struct tail (AU ABI version 1); this appex may be newer
            // than the plugin binary, so gate on the reported version
            // before reading them.
            guard truceAbiTailVersion(cb) >= 1,
                  let dict = newValue,
                  let keyCount = cb.pointee.legacy_state_key_count,
                  let keyAt = cb.pointee.legacy_state_key_at,
                  let loadForeign = cb.pointee.state_load_foreign else { return }
            for i in 0..<keyCount(ctx) {
                guard let cKey = keyAt(ctx, i),
                      let blob = dict[String(cString: cKey)] as? Data else { continue }
                let accepted = blob.withUnsafeBytes { ptr in
                    loadForeign(ctx, cKey, ptr.baseAddress?.assumingMemoryBound(to: UInt8.self), UInt32(blob.count))
                }
                if accepted != 0 {
                    syncParameterTreeFromRust()
                    return
                }
            }
        }
    }

    /// Push the Rust side's current parameter values into the AU
    /// parameter tree so host UIs reflect a state / preset load.
    private func syncParameterTreeFromRust() {
        guard let ctx = rustCtx, let cb = g_callbacks, let tree = _parameterTree else { return }
        for param in tree.allParameters {
            param.value = AUValue(cb.pointee.param_get_value(ctx, UInt32(param.address)))
        }
    }

    // MARK: Factory presets

    /// Backed by the `.trucepreset` files `cargo truce install`
    /// bundles into the framework's `Resources/Presets/` - the same
    /// library the AU v2 component serves through
    /// `kAudioUnitProperty_FactoryPresets`.
    override var factoryPresets: [AUAudioUnitPreset]? {
        guard let ctx = rustCtx, let cb = g_callbacks else { return nil }
        let n = cb.pointee.factory_preset_count(ctx)
        guard n > 0 else { return nil }
        return (0..<n).map { i in
            let preset = AUAudioUnitPreset()
            preset.number = Int(i)
            if let cName = cb.pointee.factory_preset_name(ctx, i) {
                preset.name = String(cString: cName)
            }
            return preset
        }
    }

    private var _currentPreset: AUAudioUnitPreset?
    override var currentPreset: AUAudioUnitPreset? {
        get { _currentPreset }
        set {
            guard let preset = newValue else {
                _currentPreset = nil
                return
            }
            if preset.number >= 0 {
                // Factory preset - the same apply path session
                // restore takes. A failed load (bad index, missing
                // file) leaves the current preset unchanged.
                guard let ctx = rustCtx, let cb = g_callbacks,
                      cb.pointee.factory_preset_load(ctx, UInt32(preset.number)) != 0
                else { return }
                syncParameterTreeFromRust()
                _currentPreset = preset
            } else {
                // User preset: replay the host-stored document state.
                guard let state = try? presetState(for: preset) else { return }
                fullStateForDocument = state
                _currentPreset = preset
            }
        }
    }

    // MARK: Capabilities

    override var channelCapabilities: [NSNumber]? {
        guard let d = g_descriptor?.pointee else { return nil }
        // Multiple declared bus_layouts(): one [in, out] pair per layout,
        // flattened, so the host can pick any supported channel config.
        if d.num_layouts > 0, let ins = d.layout_in_channels, let outs = d.layout_out_channels {
            var caps: [NSNumber] = []
            caps.reserveCapacity(Int(d.num_layouts) * 2)
            for i in 0..<Int(d.num_layouts) {
                caps.append(NSNumber(value: ins[i]))
                caps.append(NSNumber(value: outs[i]))
            }
            return caps
        }
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
    // Report the plugin's latency / release tail so the host aligns
    // delay compensation. Samples come from a cache the framework
    // refreshes each block; divide by the sample rate for seconds. The
    // `latency_samples` / `tail_samples` callbacks are ABI version 3,
    // so gate on the (magic-validated) reported version - this appex
    // may be newer than the framework binary it binds.
    override var latency: TimeInterval {
        guard let ctx = rustCtx, let cb = g_callbacks,
              truceAbiTailVersion(cb) >= 3, _sampleRate > 0 else { return 0 }
        return TimeInterval(cb.pointee.latency_samples(ctx)) / _sampleRate
    }
    override var tailTime: TimeInterval {
        guard let ctx = rustCtx, let cb = g_callbacks,
              truceAbiTailVersion(cb) >= 3, _sampleRate > 0 else { return 0 }
        return TimeInterval(cb.pointee.tail_samples(ctx)) / _sampleRate
    }
    override var shouldBypassEffect: Bool { get { false } set { } }

    // AUAudioUnit.latency is KVO-observed by hosts for delay
    // compensation, but the value comes from a callback (a computed
    // property) so re-assignment can't fire the notification. Called on
    // the main thread from the param-sync timer: when the framework's
    // reported latency moves, fire KVO manually so the host re-reads the
    // fresh `latency`. This is AU v3's push path - the audio thread only
    // refreshes the cache, never touches KVO.
    func notifyLatencyIfChanged() {
        guard let ctx = rustCtx, let cb = g_callbacks,
              truceAbiTailVersion(cb) >= 3 else { return }
        let now = cb.pointee.latency_samples(ctx)
        if now != _lastLatencySamples {
            _lastLatencySamples = now
            willChangeValue(forKey: "latency")
            didChangeValue(forKey: "latency")
        }
    }
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
    /// Latch flipped after the host's first `viewDidLayout` pass.
    /// On first layout Logic Pro lays our `view` out at its plug-in
    /// pane size (typically wider than the editor's natural), and
    /// propagating that to `gui_set_size` makes the editor canvas
    /// grow to fill - widgets stay at natural cell positions but
    /// the empty trailing space looks "stretched". Skipping the
    /// first layout keeps the editor at its built natural size on
    /// open; subsequent layouts (genuine user resize) still
    /// propagate.
    private var didInitialLayout = false

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
        // Without an autoresize mask, Logic Pro's bigger plug-in
        // container leaves our view pinned at its initial natural
        // size and `viewDidLayoutSubviews` never fires (our bounds
        // never change). Width / height sizable makes the view
        // follow the host container; `propagateHostResize` then
        // sees `bounds != guiPtSize` and calls `gui_set_size`, and
        // the inner `guiContainer` follows because it inherits the
        // mask too (set on the container in `setupGUIIfReady`).
        v.autoresizingMask = [.width, .height]
        #else
        // UIView always has a backing layer; set the BG directly.
        v.backgroundColor = UIColor(red: 0.15, green: 0.15, blue: 0.15, alpha: 1)
        v.autoresizingMask = [.flexibleWidth, .flexibleHeight]
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
        guiSetUp = true
        #if os(iOS)
        // Fit the editor to the host's safe-area frame so its responsive
        // layout reflows to the real device viewport.
        fitGUIToSafeArea()
        #else
        // Center the GUI in the host's view (which may be oversized).
        centerGUI()
        #endif

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
        if didInitialLayout {
            propagateHostResize()
        } else {
            didInitialLayout = true
        }
        centerGUI()
    }
    #else
    override func viewDidDisappear(_ animated: Bool) {
        super.viewDidDisappear(animated)
        teardownGUI()
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        // iOS always fits to the host's safe-area frame - unlike the macOS
        // `didInitialLayout` skip (which avoids Logic stretching the desktop
        // editor on first layout), fitting on the very first layout is the
        // whole point on iOS: the host hands us a pane sized to the device
        // and we reflow into it immediately.
        fitGUIToSafeArea()
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
        // Re-query: the editor may have clamped the request against
        // its `min_size` / `max_size` (the built-in `GridLayout`
        // does), so the stored size differs from `(newW, newH)`.
        // Using the requested values for `guiPtSize` makes
        // `centerGUI` mis-position the inner container: when host
        // bounds are *smaller* than the editor's min, the container
        // would be set to host bounds while the editor's actual
        // surface stays at min, leaving its bottom-left at the
        // host's bottom-left and the layout's TOP (GAIN header)
        // clipping off the host's top edge.
        var actW: UInt32 = 0, actH: UInt32 = 0
        cb.pointee.gui_get_size(ctx, &actW, &actH)
        guiPtSize = NSSize(width: CGFloat(max(1, actW)),
                           height: CGFloat(max(1, actH)))
        guiContainer?.frame = NSRect(origin: .zero, size: guiPtSize)
    }

    #if os(iOS)
    /// Fit the editor to the host plug-in pane's safe-area frame. AU v3
    /// hosts (GarageBand, AUM, Logic for iPad) hand us a UIView whose
    /// bounds track their pane; we drive the Rust editor to the safe-area
    /// size via `gui_set_size` and pin the container inside the safe-area
    /// insets so the editor's responsive layout reflows to the real device
    /// viewport instead of sitting at its built portrait size. When the
    /// editor opted out of resize (`gui_can_resize == 0`) we only position
    /// the natural-size container, matching the desktop `centerGUI`.
    private func fitGUIToSafeArea() {
        guard guiSetUp, let container = guiContainer, guiPtSize.width > 0 else { return }
        // `safeAreaLayoutGuide.layoutFrame` excludes the notch /
        // home-indicator insets; fall back to raw bounds before the safe
        // area resolves (it is zero until the view is in a window
        // hierarchy).
        let safeFrame = self.view.safeAreaLayoutGuide.layoutFrame
        let layoutFrame = (safeFrame.width > 0 && safeFrame.height > 0) ? safeFrame : self.view.bounds
        let hostW = layoutFrame.width
        let hostH = layoutFrame.height
        guard hostW > 0, hostH > 0 else { return }

        if let ctx = myCtx, let cb = g_callbacks,
           cb.pointee.gui_can_resize(ctx) != 0,
           (hostW, hostH) != (guiPtSize.width, guiPtSize.height) {
            let reqW = UInt32(max(1, hostW.rounded()))
            let reqH = UInt32(max(1, hostH.rounded()))
            cb.pointee.gui_set_size(ctx, reqW, reqH)
            // Re-query: the Rust side clamps the request against the
            // editor's min / max, so the size it actually adopted may
            // differ from what we asked for. Position the container to the
            // clamped size, not the request.
            var actW: UInt32 = 0, actH: UInt32 = 0
            cb.pointee.gui_get_size(ctx, &actW, &actH)
            guiPtSize = NSSize(width: CGFloat(max(1, actW)),
                               height: CGFloat(max(1, actH)))
            self.preferredContentSize = guiPtSize
        }

        // Center the editor within the safe-area frame (UIKit origin is
        // top-left). A resizable editor that filled the safe area has zero
        // offset; a fixed-size (or min-clamped) editor smaller than the
        // pane sits centered instead of pinned to a corner. `max(0, ...)`
        // keeps the top-left visible when the editor is larger than the
        // pane (better to clip the far edge than the labels).
        let offsetX = max(0, (hostW - guiPtSize.width) / 2)
        let offsetY = max(0, (hostH - guiPtSize.height) / 2)
        container.frame = NSRect(x: layoutFrame.minX + offsetX,
                                 y: layoutFrame.minY + offsetY,
                                 width: guiPtSize.width,
                                 height: guiPtSize.height)
    }
    #endif

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
        // Horizontal: center the editor when the host's view is
        // wider; clamp to 0 so the LEFT edge stays visible when the
        // host is narrower (better to clip the right edge than the
        // left where labels sit).
        let x = max(0, (hostW - guiPtSize.width) / 2)
        // Vertical: anchor to the TOP of the host view in unflipped
        // Cocoa coordinates. `(host - gui)` is positive when the
        // editor fits (centers vertically by symmetry) and negative
        // when the editor is taller than the host (e.g. Logic's UI
        // zoom shrinks the host below our editor's `min_size`); in
        // that case `(host - gui)` is negative, so `y` ends up
        // below 0 and the editor's TOP edge sits at the host's TOP
        // edge with the BOTTOM clipping off-screen. Without this,
        // `max(0, ...)` pinned the bottom-left to the host's bottom-
        // left and the GAIN header at the layout's top fell off the
        // top edge of the visible plug-in window.
        #if os(macOS)
        let y = hostH - guiPtSize.height
        #else
        // UIKit uses flipped coords (origin top-left): the natural
        // anchor for "top" is already y = 0, so the `max(0, ...)`
        // clamp keeps the top visible.
        let y = max(0, (hostH - guiPtSize.height) / 2)
        #endif
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

        // Push a latency change (if any) to the host on the same
        // main-thread tick.
        au.notifyLatencyIfChanged()
    }
}
