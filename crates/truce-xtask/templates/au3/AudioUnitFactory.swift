/// AU v3 Swift implementation — delegates all plugin logic to the Rust
/// framework via C FFI (g_callbacks function pointer table).
import os.log
import AudioToolbox

private let logger = Logger(subsystem: "com.truce.au3", category: "AUExt")
import AVFAudio
import CoreAudioKit

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

        let outputFmt = AVAudioFormat(standardFormatWithSampleRate: 44100, channels: numOut)!
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

    private func buildParameterTree() {
        guard let callbacks = g_callbacks, let ctx = rustCtx else { return }
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
        midiBuf: UnsafeMutablePointer<AuMidiEvent>
    ) -> AUAudioUnitStatus {
        if numIn > 0, let pull = pull {
            var f = AudioUnitRenderActionFlags()
            let s = pull(&f, timestamp, frameCount, 0, outputData)
            if s != noErr { return s }
        }
        var numMidi: UInt32 = 0
        var ev = events
        while let event = ev, numMidi < 256 {
            let head = event.pointee.head
            if head.eventType == .MIDI {
                let m = event.pointee.MIDI
                // Convert absolute eventSampleTime to relative offset within buffer
                let absTime = m.eventSampleTime
                let bufStart = Int64(timestamp.pointee.mSampleTime)
                let relOffset = max(0, absTime - bufStart)
                midiBuf[Int(numMidi)] = AuMidiEvent(
                    sample_offset: UInt32(min(relOffset, Int64(frameCount - 1))),
                    status: m.data.0, data1: m.length > 1 ? m.data.1 : 0,
                    data2: m.length > 2 ? m.data.2 : 0, _pad: 0)
                numMidi += 1
            } else if head.eventType == .parameter || head.eventType == .parameterRamp {
                cb.pointee.param_set_value(ctx, UInt32(event.pointee.parameter.parameterAddress),
                                           Double(event.pointee.parameter.value))
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
        cb.pointee.process(ctx, inPtrs, outPtrs, numIn, numOut, frameCount, midiBuf, numMidi)
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

        return { _, timestamp, frameCount, _, outputData, events, pull in
            return TruceAUAudioUnit.render(
                ctx: ctx, cb: cb, numIn: numIn, numOut: numOut,
                timestamp: timestamp, frameCount: frameCount,
                outputData: outputData, events: events, pull: pull,
                inPtrs: inPtrs, outPtrs: outPtrs, midiBuf: midiBuf)
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
    override var supportsUserPresets: Bool { false }
}

// MARK: - Factory

class AudioUnitFactory: AUViewController, AUAudioUnitFactory {
    private var auInstance: TruceAUAudioUnit?

    public func createAudioUnit(with componentDescription: AudioComponentDescription) throws -> AUAudioUnit {
        let au = try TruceAUAudioUnit(componentDescription: componentDescription, options: [])
        auInstance = au
        logger.info("factory createAudioUnit")
        // If the view is already loaded (host called loadView before
        // createAudioUnit), set up the GUI now that we have an instance.
        // Must dispatch to main thread — NSView operations require it.
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
                        // w/h are in logical points — use directly.
                        size = NSSize(width: CGFloat(w), height: CGFloat(h))
                    }
                }
                cb.pointee.destroy(ctx)
            }
        }
        let v = NSView(frame: NSRect(origin: .zero, size: size))
        v.wantsLayer = true
        v.layer?.backgroundColor = CGColor(red: 0.15, green: 0.15, blue: 0.15, alpha: 1)
        self.view = v
        self.preferredContentSize = size
        logger.info("loadView: \(size.width)x\(size.height)")
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        logger.info("viewDidLoad: view.frame=\(self.view.frame.width)x\(self.view.frame.height) auInstance=\(self.auInstance != nil)")
        setupGUIIfReady()
    }

    override func viewWillAppear() {
        super.viewWillAppear()
        logger.info("viewWillAppear: view.frame=\(self.view.frame.width)x\(self.view.frame.height)")
        setupGUIIfReady()
    }


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

        // w/h are in logical points — use directly.
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

    override func viewDidDisappear() {
        super.viewDidDisappear()
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

    override func viewDidLayout() {
        super.viewDidLayout()
        centerGUI()
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
