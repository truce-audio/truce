import UIKit
import AVFAudio
import AudioToolbox
import CoreAudioKit
import CoreMIDI
import os.log

private let log = Logger(subsystem: "com.truce.au3", category: "AppProbe")

private func fourcc(_ s: String) -> OSType {
    precondition(s.utf8.count == 4, "fourcc must be 4 ASCII bytes")
    var v: OSType = 0
    for b in s.utf8 { v = (v << 8) | OSType(b) }
    return v
}

private func sectionHeader(_ text: String) -> UILabel {
    let l = UILabel()
    l.text = text.uppercased()
    l.font = .preferredFont(forTextStyle: .footnote).withTraits(.traitBold)
    l.adjustsFontForContentSizeCategory = true
    l.textColor = .secondaryLabel
    return l
}

/// Dispatch one short MIDI 1.0 message from a virtual source. The
/// `AuMidiEvent` ABI is "any 3-byte MIDI message" but a plug-in
/// can legally emit System Common / Real-Time bytes (status >=
/// 0xF0) where the canonical length isn't 3 - passing those
/// through unchanged dispatches a malformed packet and the
/// receiving end's parser typically dies on the second byte. We
/// drop them at the boundary until the framework grows a
/// multi-byte status surface.
private func sendShortMidi(_ ev: AuMidiEvent, to dest: MIDIEndpointRef) {
    if ev.status >= 0xF0 {
        return
    }
    var packet = MIDIPacket()
    packet.timeStamp = 0
    packet.length = 3
    packet.data.0 = ev.status
    packet.data.1 = ev.data1
    packet.data.2 = ev.data2
    var pktList = MIDIPacketList(numPackets: 1, packet: packet)
    MIDIReceived(dest, &pktList)
}

extension UIFont {
    func withTraits(_ traits: UIFontDescriptor.SymbolicTraits) -> UIFont {
        guard let desc = fontDescriptor.withSymbolicTraits(traits) else { return self }
        return UIFont(descriptor: desc, size: 0)
    }
}

/// Root view controller - exposes `viewDidLayoutSubviews` /
/// `viewWillTransition(to:with:)` as closure callbacks so the
/// AppDelegate can drive scale-to-fit + the landscape sidebar
/// re-layout without subclassing further.
class ContainerViewController: UIViewController {
    var onLayout: (() -> Void)?
    var onWillTransition: ((CGSize) -> Void)?
    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        onLayout?()
    }
    override func viewWillTransition(
        to size: CGSize, with coordinator: UIViewControllerTransitionCoordinator
    ) {
        super.viewWillTransition(to: size, with: coordinator)
        coordinator.animate(alongsideTransition: { _ in
            self.onWillTransition?(size)
        })
    }
}

@UIApplicationMain
class AppDelegate: UIResponder, UIApplicationDelegate {
    var window: UIWindow?
    /// Root VC owning the chrome + editor preview. Held so the
    /// AppDelegate can read `view.bounds` for scale-to-fit /
    /// orientation-aware layout decisions and hook the sidebar
    /// overlay into the view hierarchy.
    var rootVC: ContainerViewController?
    /// Per-plugin "scale the editor to fit the container hero
    /// region" flag (`ios_scale_editor_to_fit` in truce.toml).
    /// Substituted at install time; the template carries the
    /// literal "true"/"false" produced by `render_app_main_swift`.
    let scaleEditorToFit: Bool = {ios_scale_editor_to_fit}
    /// Per-plugin "silence preview audio" flag
    /// (`mute_preview_output` in truce.toml). When true, the
    /// source-node render block zeros its output buffer instead of
    /// copying the plug-in's `cb.process` output - `cb.process`
    /// still runs each block so editors that visualise an input
    /// signal (analyzers, tuners, spectrum displays) keep updating
    /// from mic input without forming a feedback loop to the
    /// speakers.
    let mutePreviewOutput: Bool = {mute_preview_output}
    /// The editor's `UIView` (the one `gui_open` painted into).
    /// Held so `applyEditorScale` can apply a `CGAffineTransform`
    /// without re-doing the gui_open work. Nil until the editor
    /// is open.
    var editorContainer: UIView?
    /// Natural pixel size the editor reported via `gui_get_size`.
    /// The scale-to-fit math divides the available host bounds by
    /// this to compute the uniform scale factor.
    var editorNaturalSize: CGSize = .zero
    /// Chrome elements held on the delegate so the landscape
    /// re-layout can re-parent them between the top bar / bottom
    /// of the root view and the sidebar overlay.
    var topBar: UIStackView?
    var separator: UIView?
    var previewHost: UIView?
    /// Landscape sidebar overlay. Built lazily on the first
    /// rotation to landscape so portrait-only installs (and
    /// portrait-first launches) pay nothing.
    var hamburgerBtn: UIButton?
    var sidebarOverlay: UIView?
    var sidebarTrailingConstraint: NSLayoutConstraint?
    var sidebarTapCatcher: UIView?
    var sidebarVisible: Bool = false
    /// Vertical stack inside the sidebar that hosts the chrome
    /// (title bar, status label, Play button) while landscape is
    /// active. Held so `placeChromeInSidebar` / `placeChromeInRoot`
    /// can re-parent chrome views across rotations without having
    /// to walk the sidebar's subview list.
    var chromeStack: UIStackView?
    /// Flexible spacer that sits between the Play button and the
    /// status label in the landscape sidebar so the button stays
    /// pinned near the top (out of reach of the dynamic island
    /// which overlays the sidebar on the trailing side in
    /// landscape-left) and the status floats to the bottom. Held
    /// across rotations so each landscape entry re-uses the same
    /// spacer view instead of accumulating stray ones in the
    /// stack on each placeChromeInSidebar call.
    var sidebarChromeSpacer: UIView?
    /// Flips true once the editor-block write at the bottom of
    /// `application(_:didFinishLaunchingWithOptions:)` runs. The
    /// plug-in-independent fallback (which writes only the safe-
    /// area inset with zeroed editor frame) consults this and
    /// skips if the editor already wrote. Both blocks are
    /// `DispatchQueue.main.async`, so they fire in enqueue order
    /// - the flag is set on the first block before the second
    /// runs.
    var editorFrameWritten: Bool = false
    /// Cached `previewHost` constraint set per layout mode, so
    /// switching orientation deactivates the prior mode's anchors
    /// before activating the new ones. The portrait set references
    /// `auBtn.topAnchor`; `placeChromeInRoot` rebuilds it after
    /// moving auBtn back from the sidebar so the cached entries
    /// don't outlive the view's brief no-superview state during
    /// re-parenting.
    var previewHostPortraitConstraints: [NSLayoutConstraint] = []
    var previewHostLandscapeConstraints: [NSLayoutConstraint] = []
    /// Constraints `placeChromeInRoot` activates when moving the
    /// chrome (topBar / status / auBtn) from the sidebar back into
    /// root. Tracked so the next re-entry can deactivate the prior
    /// set before installing fresh ones - Auto Layout otherwise
    /// accumulates dead references across rotations.
    var chromeRootConstraints: [NSLayoutConstraint] = []
    /// `separator.top → topBar.bottom`. Held so `placeChromeInRoot`
    /// can rebuild it after topBar's re-parent (the original cross-
    /// view constraint is auto-removed by UIKit the moment topBar
    /// briefly leaves root's subtree during `removeFromSuperview`).
    var separatorTopConstraint: NSLayoutConstraint?
    /// Last layout mode applied; gates the re-layout work so
    /// every `viewDidLayoutSubviews` call doesn't reshuffle the
    /// view hierarchy (it can fire many times per orientation
    /// change as constraints settle).
    var lastLayoutLandscape: Bool? = nil
    // Audio test wiring lives on the delegate so the toggle button
    // can start / stop the engine across taps. AU instances must be
    // retained for the engine + scene to outlive the
    // `AVAudioUnit.instantiate` callback.
    var audioEngine: AVAudioEngine?
    var auTestButton: UIButton?
    var audioActive = false
    var auInputBuses: UInt32 = 0
    var auOutputBuses: UInt32 = 0
    var auStatusLabel: UILabel?
    // In-process plugin context - `g_callbacks.create()` returns a
    // ctx the framework owns. We drive it from an
    // `AVAudioSourceNode` render callback, skipping AVAudioUnit /
    // PluginKit / XPC entirely. The out-of-process AVAudioUnit
    // path was hitting `kAudioComponentErr_NotPermitted` (-3000)
    // on device - iOS refuses to spawn a container's own appex as
    // a host child in some signing configurations. In-process side-
    // steps the issue, and the framework is already in our address
    // space because the editor uses it.
    var inProcessCtx: UnsafeMutableRawPointer?
    var pendingNoteOn = false
    var pendingNoteOff = false
    // Effects always run live mic through the plug-in. We track
    // whether we've already prompted for / received mic permission
    // so a second Play tap doesn't re-trigger the request dance.
    var micPermissionGranted = false
    // When set, the source-node render block emits zero input
    // samples but still drives `cb.process`, so the plug-in's
    // meters can decay to zero before we tear the engine down.
    // Flag flips at the start of `stopAudio` and the real
    // teardown runs ~250 ms later.
    var fadingOut = false
    // Mic-tap ring: input-node tap writes interleaved stereo
    // Float32 frames here; the AVAudioSourceNode render block
    // drains them. NSLock contention is fine for a one-button
    // diagnostic feature.
    let micRingLock = NSLock()
    var micRing: [Float] = []
    // Core MIDI bridge - published only for MIDI processors
    // (numIn == 0, numOut == 0). The Play button toggles it.
    // While active, the plug-in appears in iOS as both a virtual
    // MIDI source (its output) and a virtual MIDI destination
    // (its input); the silent audio engine drives `cb.process` at
    // the engine block rate so the plug-in's scheduler advances.
    var midiClient: MIDIClientRef = 0
    var midiInPort: MIDIPortRef = 0
    var midiVirtualSource: MIDIEndpointRef = 0
    var midiVirtualDest: MIDIEndpointRef = 0
    var midiBridgeActive = false
    /// Per-channel scratch buffers reused by the AVAudioSourceNode
    /// render block. Pre-allocated in `prepareRenderScratch`
    /// before the engine starts so the render thread itself never
    /// allocates - the framework's RT-safety contract everywhere
    /// else (CLAP / VST3 / VST2 / AU / AAX wrappers all hoist
    /// scratch to instance fields with `EVENT_LIST_PREALLOC`-style
    /// pre-allocation; the in-app preview engine has to match).
    var renderScratchInL: UnsafeMutablePointer<Float>?
    var renderScratchInR: UnsafeMutablePointer<Float>?
    var renderScratchOutL: UnsafeMutablePointer<Float>?
    var renderScratchOutR: UnsafeMutablePointer<Float>?
    var renderScratchCapacity: Int = 0
    /// MIDI events handed to `cb.process` this block - fresh `var
    /// midi: [AuMidiEvent] = []` per render would allocate; this
    /// scratch has its backing storage reserved once at engine
    /// start and `removeAll(keepingCapacity: true)`-ed between
    /// blocks. Sized to match `midiInRing`'s 4096 cap.
    var midiInScratch: [AuMidiEvent] = []
    /// Reference count for `AVAudioSession.setActive(true/false)`.
    /// Audio + MIDI bridge can both want the session live and the
    /// two stop paths can race during a quick audio → bridge →
    /// audio toggle; an unmatched `setActive(false)` then leaves
    /// the lock-screen "now playing" widget in the wrong state for
    /// a few hundred ms. Acquired by each start, released by each
    /// teardown; the real `setActive` calls only fire on the
    /// 0→1 / 1→0 transitions.
    var audioSessionActiveCount: Int = 0
    let midiInRingLock = NSLock()
    /// Fixed-size MIDI-in ring shared between the Core MIDI input
    /// callback (high-priority MIDI thread) and the audio render
    /// thread. Pre-allocated once in `application(_:didFinishLaunching…)`
    /// so neither thread allocates inside the lock. `head` is the
    /// next slot to read; `count` is how many slots are in flight;
    /// the next write goes to `(head + count) % capacity`. On
    /// overflow we drop the oldest event (bump `head`, count stays
    /// at capacity) - preferable to blocking the MIDI thread.
    var midiInRingBuf: UnsafeMutablePointer<AuMidiEvent>?
    let midiInRingCapacity: Int = 4096
    var midiInRingHead: Int = 0
    var midiInRingCount: Int = 0
    // Held for the "About this plug-in" modal - full description
    // is one tap away rather than cluttering the default view.
    var fullDescription: String = ""

    func application(_ application: UIApplication,
                     didFinishLaunchingWithOptions launchOptions:
                        [UIApplication.LaunchOptionsKey: Any]? = nil) -> Bool {
        // Pre-allocate the MIDI-in ring up front - the Core MIDI
        // input callback fires on a high-priority MIDI thread and
        // any allocation under `midiInRingLock` would stall the
        // audio render thread waiting on the same lock. The 4096-
        // slot fixed buffer is ~32 KB; we hold it for the app's
        // lifetime (no explicit free; process exit reclaims).
        self.midiInRingBuf = UnsafeMutablePointer<AuMidiEvent>
            .allocate(capacity: self.midiInRingCapacity)

        let window = UIWindow(frame: UIScreen.main.bounds)
        let vc = ContainerViewController()
        self.rootVC = vc
        vc.onLayout = { [weak self] in self?.applyOrientationLayout() }
        vc.onWillTransition = { [weak self] _ in self?.applyOrientationLayout() }
        // Force a dark UI throughout. iOS resolves all
        // semantic colors (.label, .secondaryLabel,
        // .systemBackground, …) against this trait so labels +
        // buttons auto-flip without per-widget tinting.
        vc.overrideUserInterfaceStyle = .dark
        vc.view.backgroundColor = .systemBackground
        self.fullDescription = "{description}"

        // App-style fixed layout (no scroll view): a small top bar
        // with title + icon-style actions on the right, the editor
        // as the centered hero, two short usage lines, and a
        // single primary "Play test note" button anchored just
        // above the bottom. Nothing scrolls; the layout fits an
        // iPhone screen and adapts to size classes via auto-layout.
        let root = vc.view!
        let g = root.safeAreaLayoutGuide

        // ── Top bar ─────────────────────────────────────────────
        // Chrome (title, subtitle, icons, usage copy, status, Play
        // button) is intentionally muted one tier below the iOS
        // default so the plug-in editor itself reads as the focal
        // point. The editor renders at full opacity / saturation;
        // every surrounding element steps down to secondary /
        // tertiary label colors, which iOS resolves to ~60% / ~30%
        // white in dark mode.
        let titleLabel = UILabel()
        titleLabel.text = "{app_name}"
        titleLabel.font = .preferredFont(forTextStyle: .headline)
        titleLabel.textColor = .secondaryLabel
        titleLabel.adjustsFontForContentSizeCategory = true

        let subtitleLabel = UILabel()
        subtitleLabel.text = "by {vendor_name}"
        subtitleLabel.textColor = .tertiaryLabel
        subtitleLabel.font = .preferredFont(forTextStyle: .caption2)
        subtitleLabel.adjustsFontForContentSizeCategory = true

        let titleStack = UIStackView(arrangedSubviews: [titleLabel, subtitleLabel])
        titleStack.axis = .vertical
        titleStack.spacing = 2
        titleStack.alignment = .leading

        // Right-side icon buttons: info circle (About sheet) +
        // arrow.up.right.square (open vendor URL in Safari).
        // Mirrors the right-bar-button-item placement of a
        // UINavigationBar without having to drag in
        // UINavigationController for one screen.
        let infoBtn = iconBarButton(systemName: "info.circle") { [weak self] in
            self?.presentAbout()
        }
        let linkBtn = iconBarButton(systemName: "arrow.up.right.square") {
            if let url = URL(string: "{vendor_url}") {
                UIApplication.shared.open(url, options: [:], completionHandler: nil)
            }
        }
        let actionStack = UIStackView(arrangedSubviews: [infoBtn, linkBtn])
        actionStack.axis = .horizontal
        actionStack.spacing = 16
        actionStack.alignment = .center

        let topBar = UIStackView(arrangedSubviews: [titleStack, UIView(), actionStack])
        topBar.axis = .horizontal
        topBar.alignment = .center
        topBar.translatesAutoresizingMaskIntoConstraints = false
        root.addSubview(topBar)
        NSLayoutConstraint.activate([
            topBar.topAnchor.constraint(equalTo: g.topAnchor, constant: 8),
            topBar.leadingAnchor.constraint(equalTo: g.leadingAnchor, constant: 16),
            topBar.trailingAnchor.constraint(equalTo: g.trailingAnchor, constant: -16),
        ])
        self.topBar = topBar

        // Hairline separator under the bar - gives the chrome a
        // proper navigation-bar look without UINavigationController.
        let separator = UIView()
        separator.backgroundColor = UIColor(white: 1.0, alpha: 0.08)
        separator.translatesAutoresizingMaskIntoConstraints = false
        root.addSubview(separator)
        let separatorTop = separator.topAnchor.constraint(equalTo: topBar.bottomAnchor, constant: 10)
        NSLayoutConstraint.activate([
            separatorTop,
            separator.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            separator.trailingAnchor.constraint(equalTo: root.trailingAnchor),
            separator.heightAnchor.constraint(equalToConstant: 0.5),
        ])
        self.separator = separator
        self.separatorTopConstraint = separatorTop

        // Hamburger button - only visible in landscape, drawn over
        // the editor at the safe-area top-trailing corner. Built
        // here (hidden) so `applyOrientationLayout` can just toggle
        // `isHidden` rather than allocate on every rotation.
        let hamburger = iconBarButton(systemName: "line.3.horizontal") { [weak self] in
            self?.toggleSidebar()
        }
        hamburger.translatesAutoresizingMaskIntoConstraints = false
        hamburger.isHidden = true
        root.addSubview(hamburger)
        NSLayoutConstraint.activate([
            hamburger.topAnchor.constraint(equalTo: g.topAnchor, constant: 4),
            hamburger.trailingAnchor.constraint(equalTo: g.trailingAnchor, constant: -8),
        ])
        self.hamburgerBtn = hamburger

        // ── Bottom action area (status + button anchored to bottom) ──
        let statusLabel = UILabel()
        statusLabel.text = "Loading audio…"
        statusLabel.font = .preferredFont(forTextStyle: .caption2)
        statusLabel.adjustsFontForContentSizeCategory = true
        statusLabel.textColor = .tertiaryLabel
        statusLabel.textAlignment = .center
        statusLabel.numberOfLines = 0
        statusLabel.lineBreakMode = .byWordWrapping
        statusLabel.translatesAutoresizingMaskIntoConstraints = false
        root.addSubview(statusLabel)
        self.auStatusLabel = statusLabel

        // `.gray` reads as "this is interactive" without the loud
        // tinted-blue Apple uses for primary CTAs - keeps the
        // editor as the visual anchor.
        var btnConfig = UIButton.Configuration.gray()
        btnConfig.title = "Play"
        btnConfig.buttonSize = .large
        btnConfig.cornerStyle = .large
        btnConfig.baseForegroundColor = .label
        let auBtn = UIButton(configuration: btnConfig)
        auBtn.translatesAutoresizingMaskIntoConstraints = false
        auBtn.addAction(UIAction { [weak self] _ in
            self?.toggleAudio()
        }, for: .touchUpInside)

        root.addSubview(auBtn)
        NSLayoutConstraint.activate([
            statusLabel.bottomAnchor.constraint(equalTo: g.bottomAnchor, constant: -12),
            statusLabel.leadingAnchor.constraint(equalTo: g.leadingAnchor, constant: 16),
            statusLabel.trailingAnchor.constraint(equalTo: g.trailingAnchor, constant: -16),
            auBtn.bottomAnchor.constraint(equalTo: statusLabel.topAnchor, constant: -8),
            auBtn.heightAnchor.constraint(greaterThanOrEqualToConstant: 50),
            auBtn.leadingAnchor.constraint(equalTo: g.leadingAnchor, constant: 16),
            auBtn.trailingAnchor.constraint(equalTo: g.trailingAnchor, constant: -16),
        ])
        self.auTestButton = auBtn

        // ── Editor preview (hero - fills the centre vertically) ─
        // Usage instructions + the headphones-feedback tip live in
        // the (i) sheet so the default screen reads as just
        // [title] / [editor] / [play], with chrome receded.
        let previewHost = UIView()
        previewHost.translatesAutoresizingMaskIntoConstraints = false
        root.addSubview(previewHost)
        // Two constraint sets - portrait sandwiches the editor
        // between the separator-bottom and the Play button; the
        // landscape set pins it to the safe-area edges so the
        // editor fills the screen (chrome moves into the sidebar).
        self.previewHostPortraitConstraints = [
            previewHost.topAnchor.constraint(equalTo: separator.bottomAnchor, constant: 8),
            previewHost.bottomAnchor.constraint(equalTo: auBtn.topAnchor, constant: -16),
            previewHost.leadingAnchor.constraint(equalTo: g.leadingAnchor, constant: 16),
            previewHost.trailingAnchor.constraint(equalTo: g.trailingAnchor, constant: -16),
        ]
        // The trailing inset reserves a strip on the right for the
        // hamburger button (top-trailing of the safe area) so a
        // scale-to-fit editor never expands underneath the
        // hamburger's tap target. The hamburger button itself is
        // ~32pt wide with ~8pt margins; 56pt total is enough to
        // keep it visible regardless of font scaling. Without the
        // reservation, a scale-to-fit landscape editor renders
        // through the hamburger and the user can't open the
        // sidebar.
        self.previewHostLandscapeConstraints = [
            previewHost.topAnchor.constraint(equalTo: g.topAnchor),
            previewHost.bottomAnchor.constraint(equalTo: g.bottomAnchor),
            previewHost.leadingAnchor.constraint(equalTo: g.leadingAnchor),
            previewHost.trailingAnchor.constraint(equalTo: g.trailingAnchor, constant: -56),
        ]
        NSLayoutConstraint.activate(self.previewHostPortraitConstraints)
        self.previewHost = previewHost

        // GUI path runs first + unconditionally. `g_callbacks` is
        // populated by the framework dylib loaded in this process,
        // so the editor renders even if AVAudioUnit.instantiate
        // fails downstream (e.g. when the appex's wgpu init can't
        // come up under sandbox + simulator constraints). The
        // editor doesn't need the AVAudioUnit instance - it talks
        // to a local plugin context built from `g_callbacks.create`.
        //
        // Editor + audio share ONE plugin instance - without this
        // unification turning a knob updates only the editor's
        // private context while the audio engine plays from a
        // separate ctx with default params.
        if let cb = g_callbacks, let descriptor = g_descriptor {
            self.auInputBuses = descriptor.pointee.num_inputs
            self.auOutputBuses = descriptor.pointee.num_outputs
            self.inProcessCtx = cb.pointee.create()
            if self.inProcessCtx != nil {
                log.info("in-process AU: in=\(self.auInputBuses) out=\(self.auOutputBuses)")
                self.updateTestButtonLabel()
                self.setStatus("Ready")
            } else {
                self.setStatus("Audio unavailable: create() returned nil")
            }
        } else {
            self.setStatus("Audio unavailable: framework not loaded")
        }

        if let cb = g_callbacks, let ctx = self.inProcessCtx {
            var w: UInt32 = 0
            var h: UInt32 = 0
            cb.pointee.gui_get_size(ctx, &w, &h)
            // Use the editor's reported size verbatim. A previous
            // `max(w, 200) × max(h, 150)` floor padded the container
            // when the editor was smaller, leaving a visible black
            // margin on the right / bottom of any sub-200×150
            // editor. The fallback for a degenerate (0×0) editor is
            // 200×150 - applied only when both axes are zero, so a
            // tiny-but-valid editor renders edge-to-edge.
            let sz: CGSize
            if w == 0 && h == 0 {
                sz = CGSize(width: 200, height: 150)
            } else {
                sz = CGSize(width: CGFloat(w), height: CGFloat(h))
            }
            let container = UIView(frame: CGRect(origin: .zero, size: sz))
            container.backgroundColor = .black
            container.layer.cornerRadius = 12
            container.clipsToBounds = true
            container.translatesAutoresizingMaskIntoConstraints = false
            previewHost.addSubview(container)
            // Centre the editor inside the hero `previewHost`
            // region. previewHost's bounds vary by screen size; the
            // editor keeps its natural pixel size, so we use centre
            // anchors + fixed width/height instead of pin-to-edges
            // (which would stretch the editor and break its
            // logical-pixel coordinate space).
            NSLayoutConstraint.activate([
                container.centerXAnchor.constraint(equalTo: previewHost.centerXAnchor),
                container.centerYAnchor.constraint(equalTo: previewHost.centerYAnchor),
                container.widthAnchor.constraint(equalToConstant: sz.width),
                container.heightAnchor.constraint(equalToConstant: sz.height),
            ])
            cb.pointee.gui_open(ctx, Unmanaged.passUnretained(container).toOpaque())
            log.info("editor: gui_open(\(w)x\(h)) into UIView")
            // Keep references for `applyEditorScale`. We leave the
            // fixed-size constraints in place even when scale-to-
            // fit is active - the transform shrinks the rasterised
            // bitmap at composite time without changing the
            // editor's logical-pixel coordinate space (which the
            // CPU backend bakes into its tiny-skia Pixmap size).
            self.editorContainer = container
            self.editorNaturalSize = sz

            // After layout settles (one main-queue hop), publish the
            // editor's physical-pixel frame so `cargo truce screenshot
            // --ios --crop-mode editor` can crop the simulator-screen
            // capture down to just the plugin region. The safe-area
            // inset write that powers `--crop-mode container` happens
            // unconditionally below - it's a property of the view
            // controller, not the plug-in, so it must publish even
            // for plug-ins whose iOS editor hasn't initialised.
            DispatchQueue.main.async { [weak self] in
                vc.view.layoutIfNeeded()
                let frameInWindow = container.convert(container.bounds, to: nil)
                let s = container.window?.screen.scale ?? UIScreen.main.scale
                let px = Int(frameInWindow.minX * s)
                let py = Int(frameInWindow.minY * s)
                let pw = Int(frameInWindow.width * s)
                let ph = Int(frameInWindow.height * s)
                self?.writeFrameJson(x: px, y: py, w: pw, h: ph, scale: s,
                                     safeTopPx: Int(vc.view.safeAreaInsets.top * s))
                self?.editorFrameWritten = true
            }
        }

        window.rootViewController = vc
        window.makeKeyAndVisible()
        self.window = window

        // Plug-in-independent safe-area inset write. Runs even when
        // the editor never opens (e.g. alt-GUI backends with no iOS
        // implementation yet) so `--crop-mode container` always has
        // a status-bar height to crop. Skips when the editor block
        // above already wrote - otherwise the (0,0,0,0) frame here
        // would clobber the real editor rect.
        DispatchQueue.main.async { [weak self] in
            guard let self = self, !self.editorFrameWritten else { return }
            vc.view.layoutIfNeeded()
            let s = vc.view.window?.screen.scale ?? UIScreen.main.scale
            let safeTopPx = Int(vc.view.safeAreaInsets.top * s)
            self.writeFrameJson(x: 0, y: 0, w: 0, h: 0, scale: s, safeTopPx: safeTopPx)
        }
        return true
    }

    /// Single source of truth for the `_truce_editor_frame.json`
    /// payload. Called from up to two sites in
    /// `didFinishLaunchingWithOptions` (editor block + plug-in-
    /// independent fallback). Last writer wins; both producers
    /// agree on `safeAreaTopPx` so order doesn't affect that field.
    ///
    /// The `orientation` field tells `cargo truce screenshot` which
    /// way the UI is rendered inside the (always-portrait-physical)
    /// framebuffer: `simctl io screenshot` captures the framebuffer
    /// as-is, so a landscape-only plug-in's content appears rotated
    /// 90° within a portrait-shaped PNG. The Rust side rotates the
    /// PNG to match this orientation before applying the crop -
    /// otherwise the editor-frame coords (which are in the rendered
    /// UI space) land out of bounds against the portrait framebuffer.
    func writeFrameJson(x: Int, y: Int, w: Int, h: Int, scale: CGFloat, safeTopPx: Int) {
        let orientationName: String
        switch UIApplication.shared.connectedScenes
            .compactMap({ $0 as? UIWindowScene })
            .first?.interfaceOrientation ?? .portrait {
        case .landscapeLeft:        orientationName = "landscapeLeft"
        case .landscapeRight:       orientationName = "landscapeRight"
        case .portraitUpsideDown:   orientationName = "portraitUpsideDown"
        default:                    orientationName = "portrait"
        }
        let json = "{\"x\":\(x),\"y\":\(y),\"w\":\(w),\"h\":\(h),"
            + "\"scale\":\(scale),\"safeAreaTopPx\":\(safeTopPx),"
            + "\"orientation\":\"\(orientationName)\"}"
        if let dir = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask).first {
            let url = dir.appendingPathComponent("_truce_editor_frame.json")
            try? json.write(to: url, atomically: true, encoding: .utf8)
            log.info("frame: wrote \(url.path)")
        }
    }

    // MARK: - Audio / MIDI test toggle
    //
    // Wires three plugin shapes through the Play button:
    //
    //   - effect (numIn > 0): inputNode → AU → mainMixer. First
    //     tap triggers a `requestRecordPermission` prompt; the
    //     engine starts on grant.
    //   - instrument (numIn == 0, numOut > 0): AU → mainMixer.
    //     Plays C4 for 1.5s then auto-stops.
    //   - MIDI processor (numIn == 0, numOut == 0): toggles a
    //     Core MIDI bridge. The plug-in is published to iOS as
    //     `{app_name}` (virtual MIDI source for its output, virtual
    //     destination for its input) and a silent audio engine
    //     drives `cb.process` so the plug-in's scheduler advances.
    //
    // `audioActive` / `midiBridgeActive` mirror the engine state so
    // the button reads "Stop" while running.

    func updateTestButtonLabel() {
        guard let btn = self.auTestButton else { return }
        let isMidiProcessor = self.auInputBuses == 0 && self.auOutputBuses == 0
        btn.isEnabled = true
        if isMidiProcessor {
            btn.setTitle(self.midiBridgeActive ? "Stop MIDI bridge"
                                              : "Start MIDI bridge",
                         for: .normal)
        } else if self.audioActive {
            btn.setTitle("Stop", for: .normal)
        } else if self.auInputBuses > 0 {
            btn.setTitle("Play mic through plug-in", for: .normal)
        } else {
            btn.setTitle("Play test note", for: .normal)
        }
    }

    func toggleAudio() {
        // MIDI processors live on a different code path - no audio
        // I/O, just a Core MIDI bridge that publishes the plug-in
        // to the rest of iOS.
        if self.auInputBuses == 0 && self.auOutputBuses == 0 {
            if self.midiBridgeActive { self.stopMidiBridge() }
            else { self.startMidiBridge() }
            return
        }
        if self.audioActive {
            self.stopAudio()
            return
        }
        // Effects always run live mic through the plug-in, so the
        // first Play tap has to clear the iOS mic prompt before
        // the engine can start. iOS remembers the decision; the
        // prompt only ever shows once.
        let needsMic = self.auInputBuses > 0
        if needsMic && !self.micPermissionGranted {
            AVAudioSession.sharedInstance().requestRecordPermission { [weak self] granted in
                DispatchQueue.main.async {
                    guard let self = self else { return }
                    if granted {
                        self.micPermissionGranted = true
                        self.startAudio()
                    } else {
                        self.setStatus("Mic permission denied - enable in Settings to preview audio")
                    }
                }
            }
            return
        }
        self.startAudio()
    }

    func setStatus(_ text: String) {
        DispatchQueue.main.async {
            self.auStatusLabel?.text = text
        }
        log.info("status: \(text)")
    }

    /// Pre-allocate the per-channel float scratch + MIDI scratch
    /// the audio render block reuses each tick. Called before
    /// `engine.start()` so the first render block already has the
    /// storage. Idempotent: only re-allocates on a larger
    /// requested capacity.
    func prepareRenderScratch(maxBlockSize: Int) {
        if self.renderScratchCapacity < maxBlockSize {
            self.freeRenderScratch()
            self.renderScratchInL = UnsafeMutablePointer<Float>.allocate(capacity: maxBlockSize)
            self.renderScratchInR = UnsafeMutablePointer<Float>.allocate(capacity: maxBlockSize)
            self.renderScratchOutL = UnsafeMutablePointer<Float>.allocate(capacity: maxBlockSize)
            self.renderScratchOutR = UnsafeMutablePointer<Float>.allocate(capacity: maxBlockSize)
            self.renderScratchCapacity = maxBlockSize
        }
        // 4096 = the MIDI-in ring cap; reserving here means the
        // first `append(contentsOf:)` of a full ring doesn't grow.
        if self.midiInScratch.capacity < 4096 {
            self.midiInScratch.reserveCapacity(4096)
        }
    }

    /// Release the render-scratch allocations. Safe to call from
    /// the main thread when no render block is in flight (engine
    /// stopped). Called from both audio + MIDI bridge teardown.
    func freeRenderScratch() {
        self.renderScratchInL?.deallocate(); self.renderScratchInL = nil
        self.renderScratchInR?.deallocate(); self.renderScratchInR = nil
        self.renderScratchOutL?.deallocate(); self.renderScratchOutL = nil
        self.renderScratchOutR?.deallocate(); self.renderScratchOutR = nil
        self.renderScratchCapacity = 0
    }

    /// Bump the audio-session refcount, calling the real
    /// `setActive(true)` only on the 0→1 transition. Throws if
    /// the underlying activation fails (caller's responsibility
    /// to decide whether to release the increment).
    func acquireAudioSession() throws {
        if self.audioSessionActiveCount == 0 {
            try AVAudioSession.sharedInstance().setActive(true)
        }
        self.audioSessionActiveCount += 1
    }

    /// Drop the audio-session refcount, calling the real
    /// `setActive(false)` only on the final release. Idempotent
    /// past zero so a paranoid double-call from cleanup paths
    /// can't drive the count negative.
    func releaseAudioSession() {
        if self.audioSessionActiveCount > 0 {
            self.audioSessionActiveCount -= 1
        }
        if self.audioSessionActiveCount == 0 {
            try? AVAudioSession.sharedInstance().setActive(false)
        }
    }

    func startAudio() {
        guard let cb = g_callbacks, let ctx = self.inProcessCtx else {
            self.setStatus("Audio unavailable: framework not loaded")
            return
        }

        // Activate the audio session before starting the engine.
        // Effects route the device mic through the plug-in, so they
        // need `.playAndRecord` to enable inputNode. Instruments
        // are output-only and use `.playback`. `.mixWithOthers`
        // keeps the session co-operative with other apps.
        // `.defaultToSpeaker` is the standard "use loudspeaker, not
        // earpiece" preference. `.allowBluetoothA2DP` is what
        // actually gets output to AirPods / Bluetooth headphones -
        // without it iOS treats BT as input-only (HFP) and the
        // tone still comes out of the phone speaker even when
        // headphones are paired. Wired EarPods preempt unconditionally.
        let session = AVAudioSession.sharedInstance()
        let isEffect = self.auInputBuses > 0
        do {
            if isEffect {
                try session.setCategory(.playAndRecord, mode: .default,
                                        options: [.mixWithOthers,
                                                  .defaultToSpeaker,
                                                  .allowBluetoothA2DP,
                                                  .allowAirPlay])
            } else {
                try session.setCategory(.playback, mode: .default,
                                        options: [.mixWithOthers])
            }
            try self.acquireAudioSession()
        } catch {
            log.error("audio session setup: \(error.localizedDescription)")
            self.setStatus("Audio session error: \(error.localizedDescription)")
            return
        }

        // Reset the plugin to a known state at the engine's sample
        // rate + block size. Without `reset()` the plugin's
        // smoothers / sample-rate-aware DSP state is uninitialised
        // and audio may glitch or be silent on the first block.
        let sr = AVAudioSession.sharedInstance().sampleRate
        let maxBlock = 4096
        cb.pointee.reset(ctx, sr, UInt32(maxBlock))
        self.prepareRenderScratch(maxBlockSize: maxBlock)

        let engine = AVAudioEngine()
        // 2-channel float32 format matches the AU C ABI exactly
        // (`float **` per-channel, non-interleaved) so we can copy
        // straight from AVAudioPCMBuffer into the in-process plug-in.
        let fmt = AVAudioFormat(commonFormat: .pcmFormatFloat32,
                                sampleRate: sr,
                                channels: 2,
                                interleaved: false)!
        let inputBuses = self.auInputBuses
        let outputBuses = self.auOutputBuses
        let noteOnRef = UnsafeMutablePointer<Bool>.allocate(capacity: 1)
        let noteOffRef = UnsafeMutablePointer<Bool>.allocate(capacity: 1)
        noteOnRef.pointee = !isEffect
        noteOffRef.pointee = false
        // Source node feeds the speakers. For effects it drains the
        // mic-tap ring buffer and routes it through the in-process
        // AU's `cb.process`. For instruments it injects MIDI events
        // and lets `cb.process` produce the audio. Per-block buffers
        // (`inL`/`inR`/`outL`/`outR`, MIDI scratch) come from the
        // `AppDelegate`'s pre-allocated `renderScratch*` storage so
        // the render thread itself never allocates.
        let sourceNode = AVAudioSourceNode(format: fmt) { _, _, frameCount, abl in
            // SAFETY: the buffer list is non-nil for the lifetime
            // of the render callback; iOS sizes its channel array
            // to match our format (2 channels). The scratch
            // pointers are non-nil because `prepareRenderScratch`
            // ran before `engine.start()` and they live until
            // `freeRenderScratch` in teardown (after the engine
            // has stopped emitting render callbacks).
            let bufList = UnsafeMutableAudioBufferListPointer(abl)
            let n = Int(frameCount)
            guard let inL = self.renderScratchInL,
                  let inR = self.renderScratchInR,
                  let outL = self.renderScratchOutL,
                  let outR = self.renderScratchOutR else {
                return noErr
            }

            // During the fade-out tail (between Stop tap and real
            // engine teardown) the input is silence - `cb.process`
            // still runs, so the plug-in's meters can decay to
            // zero before we kill the engine.
            if self.fadingOut {
                for i in 0..<n { inL[i] = 0; inR[i] = 0 }
            } else if isEffect {
                // Drain the mic-tap ring into the input scratch.
                // If there's not enough data yet (engine warming
                // up / tap hasn't fired) the remainder stays at
                // 0 - pre-roll silence is preferable to glitching.
                self.micRingLock.lock()
                let avail = min(self.micRing.count / 2, n)
                for i in 0..<avail {
                    inL[i] = self.micRing[i * 2]
                    inR[i] = self.micRing[i * 2 + 1]
                }
                if avail > 0 {
                    self.micRing.removeFirst(avail * 2)
                }
                self.micRingLock.unlock()
                for i in avail..<n { inL[i] = 0; inR[i] = 0 }
            } else {
                // Instrument: zeroed input (the plug-in won't read
                // it; numIn == 0). Empty for completeness.
                for i in 0..<n { inL[i] = 0; inR[i] = 0 }
            }
            for i in 0..<n { outL[i] = 0; outR[i] = 0 }

            // Build the MIDI event list. We push a NoteOn the first
            // time `cb.process` runs after a `Start` tap and a
            // NoteOff on Stop. For instruments we additionally drain
            // the Core MIDI bridge ring so external keyboards / other
            // iOS MIDI apps can play the plug-in alongside the test
            // note.
            self.midiInScratch.removeAll(keepingCapacity: true)
            if noteOnRef.pointee {
                self.midiInScratch.append(AuMidiEvent(
                    sample_offset: 0,
                    status: 0x90, data1: 60, data2: 95, _pad: 0))
                noteOnRef.pointee = false
            }
            if noteOffRef.pointee {
                self.midiInScratch.append(AuMidiEvent(
                    sample_offset: 0,
                    status: 0x80, data1: 60, data2: 0, _pad: 0))
                noteOffRef.pointee = false
            }
            if !isEffect {
                self.drainMidiInRing()
            }

            // Hand off to the framework: per-channel float pointers
            // for input + output, MIDI events, no transport.
            var inPtrs: [UnsafePointer<Float>?] = [
                UnsafePointer(inL), UnsafePointer(inR)
            ]
            var outPtrs: [UnsafeMutablePointer<Float>?] = [outL, outR]
            inPtrs.withUnsafeMutableBufferPointer { inBuf in
                outPtrs.withUnsafeMutableBufferPointer { outBuf in
                    self.midiInScratch.withUnsafeBufferPointer { midiBuf in
                        cb.pointee.process(
                            ctx,
                            inBuf.baseAddress,
                            outBuf.baseAddress,
                            inputBuses, outputBuses,
                            UInt32(n),
                            midiBuf.baseAddress, UInt32(self.midiInScratch.count),
                            nil, 0,
                            // No host-side parameter automation in the
                            // iOS container app's preview engine - the
                            // app is its own "host" and parameter
                            // changes come via the AUParameterTree
                            // observer (synchronous, no per-sample
                            // timing). Pass nil/0 like the AU v2
                            // shim does.
                            nil, 0,
                            nil)
                    }
                }
            }

            // Copy AU output into AVAudioEngine's output buffer.
            // When `mutePreviewOutput` is set the plug-in's output
            // is discarded and we hand silence to the mixer instead.
            // `cb.process` already ran above, so the editor's
            // visualisation (FFT, meters, ...) sees the input it
            // needs; we just refuse to close the loop to the
            // speakers.
            for ch in 0..<bufList.count {
                let dst = bufList[ch].mData!.assumingMemoryBound(to: Float.self)
                if self.mutePreviewOutput {
                    for i in 0..<n { dst[i] = 0 }
                } else {
                    let src = ch == 0 ? outL : outR
                    for i in 0..<n { dst[i] = src[i] }
                }
            }
            // Drain plug-in MIDI output → virtual MIDI source so
            // other iOS apps subscribed to "{app_name}" see the
            // events. Only relevant for plug-ins that emit MIDI
            // (arpeggios, chord generators, …); virtualSource stays
            // 0 for pure synths and we skip the loop.
            let virtualSource = self.midiVirtualSource
            if virtualSource != 0 {
                let outCount = cb.pointee.output_event_count(ctx)
                for i in 0..<outCount {
                    var ev = AuMidiEvent(sample_offset: 0, status: 0,
                                         data1: 0, data2: 0, _pad: 0)
                    cb.pointee.output_event_at(ctx, i, &ev)
                    sendShortMidi(ev, to: virtualSource)
                }
            }
            return noErr
        }
        engine.attach(sourceNode)
        engine.connect(sourceNode, to: engine.mainMixerNode, format: fmt)

        // Instruments also get the Core MIDI bridge: external MIDI
        // keyboards / other iOS MIDI apps can play the plug-in
        // alongside the test note. Effects keep mic-only routing.
        if !isEffect {
            _ = self.setupMidiClient()
        }

        // Wire mic capture into the ring buffer the source node
        // drains. We pull a tap off the engine's `inputNode` rather
        // than routing it through the graph so the source node
        // stays the sole input to `cb.process` - mic samples flow
        // into the ring, source pulls them out at render time.
        if isEffect {
            self.micRingLock.lock()
            self.micRing.removeAll(keepingCapacity: true)
            self.micRingLock.unlock()
            let inputNode = engine.inputNode
            let micFmt = inputNode.outputFormat(forBus: 0)
            inputNode.installTap(onBus: 0, bufferSize: 1024, format: micFmt) { [weak self] buffer, _ in
                guard let self = self else { return }
                guard let data = buffer.floatChannelData else { return }
                let frames = Int(buffer.frameLength)
                // Stereo-ise to match the source node's format.
                let chCount = Int(buffer.format.channelCount)
                self.micRingLock.lock()
                for i in 0..<frames {
                    let l = data[0][i]
                    let r = chCount > 1 ? data[1][i] : l
                    self.micRing.append(l)
                    self.micRing.append(r)
                }
                // Bound the ring so a stalled source node doesn't
                // pile up unbounded memory - drop oldest beyond
                // ~500 ms of stereo audio (44.1 kHz × 2ch × 0.5s).
                let cap = 44100
                if self.micRing.count > cap * 2 {
                    let trim = self.micRing.count - cap * 2
                    self.micRing.removeFirst(trim)
                }
                self.micRingLock.unlock()
            }
        }

        do {
            try engine.start()
            self.audioEngine = engine
            self.audioActive = true
            self.updateTestButtonLabel()
            self.setStatus(isEffect
                ? "Playing mic through plug-in"
                : "Playing C4 through instrument")

            // For instruments, auto-release the held note after 1.5s
            // so a tap doesn't hang the synth in note-on land. The
            // user can re-tap to play another note.
            if !isEffect {
                DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) { [weak self] in
                    noteOffRef.pointee = true
                    // Audio keeps running so the envelope's release
                    // tail is audible; the user taps Stop to cut it.
                    self?.setStatus("Note released - tap Stop to silence")
                }
            }
        } catch {
            log.error("engine start: \(error.localizedDescription)")
            self.setStatus("Engine start failed: \(error.localizedDescription)")
        }
    }

    func stopAudio() {
        if self.audioEngine == nil { return }
        // Two-step teardown: flag the source node to emit silence
        // (the real engine keeps running so `cb.process` ticks
        // and the plug-in's meters integrate down to zero), then
        // tear everything down on a 250 ms delay. Without the
        // fade the meter is stuck at whatever level it had when
        // we cut audio - the editor's display loop has no way to
        // animate it back to zero without DSP feeding it.
        self.fadingOut = true
        self.audioActive = false
        self.updateTestButtonLabel()
        self.setStatus("Stopping…")
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.25) { [weak self] in
            self?.teardownAudio()
        }
    }

    func teardownAudio() {
        if let engine = self.audioEngine {
            // Removing the input tap before stop avoids the tap's
            // ring buffer continuing to fill after the engine is
            // torn down. Only effects install one - instruments
            // never call `installTap` so skip the removal there
            // to avoid a "tap not installed" warning.
            if self.auInputBuses > 0 {
                engine.inputNode.removeTap(onBus: 0)
            }
            engine.stop()
        }
        self.audioEngine = nil
        self.fadingOut = false
        self.micRingLock.lock()
        self.micRing.removeAll(keepingCapacity: true)
        self.micRingLock.unlock()
        // Tear down the MIDI bridge alongside the audio engine.
        // Instruments set it up in startAudio; effects never did.
        // `teardownMidiBridge` is a no-op if the client is 0.
        self.teardownMidiBridge()
        self.freeRenderScratch()
        self.releaseAudioSession()
        self.setStatus("Ready")
    }

    // MARK: - Core MIDI bridge (MIDI processors)
    //
    // For plug-ins with no audio I/O (numIn == 0, numOut == 0) we
    // become a real iOS MIDI citizen instead of trying to make
    // audio out of nothing: publish a virtual source for the
    // plug-in's MIDI output, a virtual destination for its MIDI
    // input, and connect every system source (USB / Bluetooth MIDI
    // keyboards, virtual sources from other apps) into our input
    // port. A silent AVAudioEngine drives `cb.process` at the
    // engine's block rate so the plug-in's own scheduler
    // (arpeggio steps, etc.) ticks forward.

    /// Publish the plug-in as a virtual MIDI source + destination
    /// and connect every system source into our input port.
    /// Idempotent in the sense that subsequent calls while already
    /// set up will no-op via the `midiClient != 0` guard. Returns
    /// `false` on hard failure (status text set).
    func setupMidiClient() -> Bool {
        if self.midiClient != 0 { return true }
        let clientName = "{app_name}" as CFString
        // The notify block fires on device list changes (Bluetooth
        // pair / unpair, USB attach / detach) so we can re-connect
        // new sources mid-session.
        let clientStatus = MIDIClientCreateWithBlock(clientName, &self.midiClient) { [weak self] _ in
            self?.connectAllMidiSources()
        }
        if clientStatus != noErr {
            self.setStatus("MIDI client create failed (OSStatus \(clientStatus))")
            return false
        }
        // Input port: receives MIDI from every system source we
        // connect below. The block runs on a high-priority MIDI
        // thread, so we just enqueue into a lock-guarded ring and
        // let the audio render block drain it.
        MIDIInputPortCreateWithBlock(self.midiClient, "in" as CFString, &self.midiInPort) { [weak self] pktListPtr, _ in
            self?.enqueueMidiIn(pktListPtr)
        }
        self.connectAllMidiSources()
        // Virtual destination - other apps see "{app_name}" in
        // their MIDI destination list and can send to it. Routes
        // into the same ring as connected sources.
        MIDIDestinationCreateWithBlock(self.midiClient, clientName, &self.midiVirtualDest) { [weak self] pktListPtr, _ in
            self?.enqueueMidiIn(pktListPtr)
        }
        // Virtual source - only published if the plug-in actually
        // emits MIDI. Pure audio effects shouldn't show up as a
        // ghost source in other apps' MIDI input pickers.
        let hasMidiOut = (g_descriptor?.pointee.has_midi_output ?? 0) != 0
        if hasMidiOut {
            MIDISourceCreate(self.midiClient, clientName, &self.midiVirtualSource)
        }
        return true
    }

    func startMidiBridge() {
        guard let cb = g_callbacks, let ctx = self.inProcessCtx else {
            self.setStatus("MIDI bridge unavailable: framework not loaded")
            return
        }
        if !self.setupMidiClient() { return }

        // Silent audio engine: gives us a steady cb.process tick at
        // the engine's block rate. `.playback` so iOS doesn't
        // route through the mic, `.mixWithOthers` so we don't
        // pre-empt other audio apps.
        let session = AVAudioSession.sharedInstance()
        do {
            try session.setCategory(.playback, mode: .default, options: [.mixWithOthers])
            try self.acquireAudioSession()
        } catch {
            log.error("MIDI bridge session setup: \(error.localizedDescription)")
            self.setStatus("MIDI bridge session error: \(error.localizedDescription)")
            self.teardownMidiBridge()
            return
        }
        let sr = AVAudioSession.sharedInstance().sampleRate
        let maxBlock = 4096
        cb.pointee.reset(ctx, sr, UInt32(maxBlock))
        self.prepareRenderScratch(maxBlockSize: maxBlock)

        let engine = AVAudioEngine()
        let fmt = AVAudioFormat(commonFormat: .pcmFormatFloat32,
                                sampleRate: sr,
                                channels: 2,
                                interleaved: false)!
        let virtualSource = self.midiVirtualSource
        let sourceNode = AVAudioSourceNode(format: fmt) { _, _, frameCount, abl in
            let bufList = UnsafeMutableAudioBufferListPointer(abl)
            let n = Int(frameCount)
            // Output silence - the plug-in has no audio buses,
            // we're just here to drive `cb.process`.
            for ch in 0..<bufList.count {
                let dst = bufList[ch].mData!.assumingMemoryBound(to: Float.self)
                for i in 0..<n { dst[i] = 0 }
            }
            // Move the MIDI-in ring into the persistent scratch.
            // `drainMidiInRing` is allocation-free because the
            // scratch backing is reserved in `prepareRenderScratch`.
            self.midiInScratch.removeAll(keepingCapacity: true)
            self.drainMidiInRing()
            self.midiInScratch.withUnsafeBufferPointer { midiBuf in
                cb.pointee.process(ctx,
                    nil, nil,
                    0, 0,
                    UInt32(n),
                    midiBuf.baseAddress, UInt32(self.midiInScratch.count),
                    nil, 0,
                    // See the matching nil/0 in the audio-output
                    // render path above re: no host-side parameter
                    // automation in the container app.
                    nil, 0,
                    nil)
            }
            // Drain the plug-in's MIDI output. Each event becomes
            // a one-packet MIDIPacketList delivered via MIDIReceived,
            // which queues it for any app subscribed to our virtual
            // source. MIDIReceived is documented as RT-safe.
            if virtualSource != 0 {
                let outCount = cb.pointee.output_event_count(ctx)
                for i in 0..<outCount {
                    var ev = AuMidiEvent(sample_offset: 0, status: 0,
                                         data1: 0, data2: 0, _pad: 0)
                    cb.pointee.output_event_at(ctx, i, &ev)
                    sendShortMidi(ev, to: virtualSource)
                }
            }
            return noErr
        }
        engine.attach(sourceNode)
        engine.connect(sourceNode, to: engine.mainMixerNode, format: fmt)
        do {
            try engine.start()
            self.audioEngine = engine
            self.midiBridgeActive = true
            self.updateTestButtonLabel()
            let hasOut = self.midiVirtualSource != 0
            self.setStatus(hasOut
                ? "MIDI bridge active - “{app_name}” published as MIDI source + destination"
                : "MIDI bridge active - “{app_name}” published as MIDI destination")
        } catch {
            log.error("MIDI bridge engine start: \(error.localizedDescription)")
            self.setStatus("MIDI bridge start failed: \(error.localizedDescription)")
            self.teardownMidiBridge()
            // We bumped the refcount in `acquireAudioSession` above
            // but the engine failed to start, so release back to
            // balance - otherwise a later `stopMidiBridge` decrement
            // would leave the count permanently above zero.
            self.releaseAudioSession()
        }
    }

    func stopMidiBridge() {
        if let engine = self.audioEngine { engine.stop() }
        self.audioEngine = nil
        self.teardownMidiBridge()
        self.freeRenderScratch()
        self.releaseAudioSession()
        self.midiBridgeActive = false
        self.updateTestButtonLabel()
        self.setStatus("Ready")
    }

    /// Tear down MIDI endpoints + client. Safe to call multiple
    /// times - handles are zeroed after dispose so a second call
    /// no-ops on the already-disposed slots.
    func teardownMidiBridge() {
        if self.midiVirtualSource != 0 {
            MIDIEndpointDispose(self.midiVirtualSource); self.midiVirtualSource = 0
        }
        if self.midiVirtualDest != 0 {
            MIDIEndpointDispose(self.midiVirtualDest); self.midiVirtualDest = 0
        }
        if self.midiInPort != 0 {
            MIDIPortDispose(self.midiInPort); self.midiInPort = 0
        }
        if self.midiClient != 0 {
            MIDIClientDispose(self.midiClient); self.midiClient = 0
        }
        self.midiInRingLock.lock()
        self.midiInRingHead = 0
        self.midiInRingCount = 0
        self.midiInRingLock.unlock()
    }

    /// Connect every currently-available system MIDI source to our
    /// input port. Called on bridge start and again whenever the
    /// MIDI client's notify block fires (device added / removed).
    /// `MIDIPortConnectSource` is idempotent for already-connected
    /// sources, so re-running on a notify is harmless.
    func connectAllMidiSources() {
        let count = MIDIGetNumberOfSources()
        for i in 0..<count {
            let src = MIDIGetSource(i)
            if src != 0 {
                MIDIPortConnectSource(self.midiInPort, src, nil)
            }
        }
    }

    /// Parse a legacy `MIDIPacketList` (one callback may carry
    /// multiple packets; multi-byte messages get extracted as the
    /// first 3 bytes - multi-byte SysEx is dropped at this seam).
    /// Allocation-free: byte extract via `withUnsafeBytes` without
    /// an intermediate `Array`, push into the pre-allocated fixed
    /// ring under lock.
    func enqueueMidiIn(_ pktListPtr: UnsafePointer<MIDIPacketList>) {
        let count = Int(pktListPtr.pointee.numPackets)
        guard count > 0, let buf = self.midiInRingBuf else { return }
        let cap = self.midiInRingCapacity
        var packetPtr: UnsafePointer<MIDIPacket> = pktListPtr.pointer(to: \.packet)!
        self.midiInRingLock.lock()
        for _ in 0..<count {
            let length = Int(packetPtr.pointee.length)
            if length >= 1 {
                let (status, data1, data2): (UInt8, UInt8, UInt8) =
                    withUnsafeBytes(of: packetPtr.pointee.data) { raw in
                        (raw[0],
                         length > 1 ? raw[1] : 0,
                         length > 2 ? raw[2] : 0)
                    }
                // Drop-oldest-on-overflow: keep producer wait-free
                // beyond the lock itself.
                if self.midiInRingCount == cap {
                    self.midiInRingHead = (self.midiInRingHead + 1) % cap
                    self.midiInRingCount -= 1
                }
                let tail = (self.midiInRingHead + self.midiInRingCount) % cap
                buf[tail] = AuMidiEvent(
                    sample_offset: 0,
                    status: status, data1: data1, data2: data2, _pad: 0)
                self.midiInRingCount += 1
            }
            packetPtr = UnsafePointer(MIDIPacketNext(packetPtr))
        }
        self.midiInRingLock.unlock()
    }

    /// Drain the MIDI-in ring into `midiInScratch`, called from
    /// the audio render thread. Holds the lock only for the copy
    /// loop; `midiInScratch` has `reserveCapacity(4096)` so the
    /// per-event `append` doesn't re-allocate.
    func drainMidiInRing() {
        guard let buf = self.midiInRingBuf else { return }
        let cap = self.midiInRingCapacity
        self.midiInRingLock.lock()
        var idx = self.midiInRingHead
        for _ in 0..<self.midiInRingCount {
            self.midiInScratch.append(buf[idx])
            idx = (idx + 1) % cap
        }
        self.midiInRingHead = idx
        self.midiInRingCount = 0
        self.midiInRingLock.unlock()
    }

    // MARK: - Secondary actions
    //
    // The full description, hidden behind an "About this plug-in"
    // row, presents as a sheet so the default UI stays
    // editor-first. Apple's review guideline is satisfied by the
    // editor + audio test being immediately accessible; long-form
    // copy doesn't need top billing.

    func presentAbout() {
        let sheet = UIViewController()
        sheet.overrideUserInterfaceStyle = .dark
        sheet.view.backgroundColor = .systemBackground

        // Plug-in description (from truce.toml) on top, followed by
        // a "How to use" section that explains the DAW-host flow and
        // the in-app preview. For effects we also surface the
        // headphones-feedback tip - irrelevant for instruments.
        let descLabel = UILabel()
        descLabel.text = self.fullDescription
        descLabel.numberOfLines = 0
        descLabel.font = .preferredFont(forTextStyle: .body)
        descLabel.textColor = .label

        let usageHeader = UILabel()
        usageHeader.text = "How to use"
        usageHeader.font = .preferredFont(forTextStyle: .headline)
        usageHeader.textColor = .label

        let usageBody = UILabel()
        usageBody.numberOfLines = 0
        usageBody.font = .preferredFont(forTextStyle: .body)
        usageBody.textColor = .secondaryLabel
        let isMidiProcessor = self.auInputBuses == 0 && self.auOutputBuses == 0
        let isInstrument = self.auInputBuses == 0 && self.auOutputBuses > 0
        var usageText: String
        if isMidiProcessor {
            usageText = "Load “{app_name}” inside an AUv3 host like "
                + "GarageBand, AUM, Cubasis, or Logic Pro for iPad - or "
                + "tap Start MIDI bridge to publish it as a virtual MIDI "
                + "source and destination that any other iOS MIDI app "
                + "can connect to."
        } else if isInstrument {
            usageText = "Load “{app_name}” inside an AUv3 host like "
                + "GarageBand, AUM, Cubasis, or Logic Pro for iPad - or "
                + "tap Play to hear a test note. While preview is "
                + "active, “{app_name}” is also published as a virtual "
                + "MIDI destination, so external MIDI keyboards and "
                + "other iOS MIDI apps can play the instrument live."
        } else {
            usageText = "Load “{app_name}” inside an AUv3 host like "
                + "GarageBand, AUM, Cubasis, or Logic Pro for iPad - or "
                + "tap Play to preview it without a DAW."
        }
        if self.auInputBuses > 0 {
            usageText += "\n\nTip: use headphones or earbuds when previewing - "
                + "the mic will pick up the loudspeaker and feed back otherwise."
        }
        usageBody.text = usageText

        // Explicit Close button - iOS hides the swipe-grabber on
        // landscape iPhone where sheets present full-screen, and
        // the navigation chrome-less `UIViewController` we present
        // has no implicit dismiss control. Without this the user
        // is stuck on the About screen until they kill the app.
        var closeCfg = UIButton.Configuration.plain()
        closeCfg.image = UIImage(systemName: "xmark.circle.fill")?
            .applyingSymbolConfiguration(.init(pointSize: 28, weight: .regular))
        closeCfg.contentInsets = NSDirectionalEdgeInsets(top: 4, leading: 4, bottom: 4, trailing: 4)
        let closeBtn = UIButton(configuration: closeCfg)
        closeBtn.tintColor = .secondaryLabel
        closeBtn.translatesAutoresizingMaskIntoConstraints = false
        closeBtn.addAction(UIAction { [weak sheet] _ in
            sheet?.dismiss(animated: true)
        }, for: .touchUpInside)
        sheet.view.addSubview(closeBtn)

        let stack = UIStackView(arrangedSubviews: [descLabel, usageHeader, usageBody])
        stack.axis = .vertical
        stack.spacing = 12
        stack.setCustomSpacing(20, after: descLabel)
        stack.translatesAutoresizingMaskIntoConstraints = false
        sheet.view.addSubview(stack)

        let g = sheet.view.safeAreaLayoutGuide
        NSLayoutConstraint.activate([
            closeBtn.topAnchor.constraint(equalTo: g.topAnchor, constant: 8),
            closeBtn.trailingAnchor.constraint(equalTo: g.trailingAnchor, constant: -12),
            stack.topAnchor.constraint(equalTo: closeBtn.bottomAnchor, constant: 8),
            stack.leadingAnchor.constraint(equalTo: g.leadingAnchor, constant: 20),
            stack.trailingAnchor.constraint(equalTo: g.trailingAnchor, constant: -20),
        ])
        if let sheetPresentation = sheet.sheetPresentationController {
            sheetPresentation.detents = [.medium(), .large()]
            sheetPresentation.prefersGrabberVisible = true
        }
        self.window?.rootViewController?.present(sheet, animated: true)
    }

    /// SF Symbol icon button sized for the top bar - matches the
    /// look of a `UIBarButtonItem` (28pt tap target, secondary-label
    /// tint) without needing UINavigationController.
    func iconBarButton(systemName: String, action: @escaping () -> Void) -> UIButton {
        var cfg = UIButton.Configuration.plain()
        cfg.image = UIImage(systemName: systemName)?
            .applyingSymbolConfiguration(.init(pointSize: 18, weight: .regular))
        cfg.contentInsets = NSDirectionalEdgeInsets(top: 6, leading: 6, bottom: 6, trailing: 6)
        let btn = UIButton(configuration: cfg)
        btn.tintColor = .secondaryLabel
        btn.addAction(UIAction { _ in action() }, for: .touchUpInside)
        return btn
    }

    // MARK: - Orientation-aware layout
    //
    // Portrait: chrome stacks vertically - top bar / editor /
    // status / Play button. The editor lives in `previewHost`,
    // which sandwiches between the chrome bands.
    //
    // Landscape: chrome moves behind a hamburger overlay. The
    // editor fills the full safe-area rect; tapping the
    // hamburger in the top-right slides a sidebar in from the
    // right carrying the same title / status / Play button views.
    //
    // Scale-to-fit (`scaleEditorToFit`) is independent - applies
    // a `CGAffineTransform` to the editor view so an oversize
    // editor shrinks uniformly to fit `previewHost.bounds`.

    /// Called from `ContainerViewController.viewDidLayoutSubviews`
    /// and after orientation transitions. Idempotent - short-
    /// circuits on the second consecutive call with the same
    /// orientation so repeated layout passes don't re-shuffle
    /// the view hierarchy.
    func applyOrientationLayout() {
        guard let root = self.rootVC?.view else { return }
        let isLandscape = root.bounds.width > root.bounds.height
        let previousLandscape = self.lastLayoutLandscape
        if previousLandscape != isLandscape {
            self.lastLayoutLandscape = isLandscape
            if isLandscape {
                NSLayoutConstraint.deactivate(self.previewHostPortraitConstraints)
                NSLayoutConstraint.activate(self.previewHostLandscapeConstraints)
                self.topBar?.isHidden = true
                self.separator?.isHidden = true
                self.auTestButton?.isHidden = true
                self.auStatusLabel?.isHidden = true
                self.hamburgerBtn?.isHidden = false
                self.buildSidebarIfNeeded()
                self.placeChromeInSidebar()
                // `previewHost` was added to `root` after the
                // hamburger, so by default it draws OVER it and
                // the tap target is hidden. Hoist hamburger +
                // sidebar to the front of root's subview list so
                // they sit above the editor in landscape.
                if let hamburger = self.hamburgerBtn {
                    root.bringSubviewToFront(hamburger)
                }
                if let tap = self.sidebarTapCatcher {
                    root.bringSubviewToFront(tap)
                }
                if let sidebar = self.sidebarOverlay {
                    root.bringSubviewToFront(sidebar)
                }
            } else {
                // Returning to portrait: pull chrome back into root
                // BEFORE re-activating the portrait constraint set.
                // `previewHostPortraitConstraints` anchors
                // `previewHost.bottom` to `auBtn.topAnchor`; with
                // auBtn still parked in the off-screen sidebar, the
                // anchor resolves up near the safe-area top and
                // previewHost collapses into a thin band - the
                // "halfway up and cut off" failure mode of any
                // portrait → landscape → portrait cycle.
                //
                // The `previousLandscape == true` gate matters: on
                // the very first layout pass `previousLandscape` is
                // nil and chrome is still in its initial root-side
                // position from `application(_:didFinishLaunching…)`
                // - running placeChromeInRoot there would
                // remove-and-re-add chrome that's already correctly
                // parented, auto-removing the still-active
                // `separator.top → topBar.bottom` and
                // `previewHost.bottom → auBtn.top` cross-view
                // constraints in the process.
                if previousLandscape == true {
                    self.placeChromeInRoot()
                }
                NSLayoutConstraint.deactivate(self.previewHostLandscapeConstraints)
                NSLayoutConstraint.activate(self.previewHostPortraitConstraints)
                self.topBar?.isHidden = false
                self.separator?.isHidden = false
                self.auTestButton?.isHidden = false
                self.auStatusLabel?.isHidden = false
                self.hamburgerBtn?.isHidden = true
                self.hideSidebar(animated: false)
            }
        }
        self.applyEditorScale()
    }

    /// Re-parent the chrome (`topBar`, `auStatusLabel`,
    /// `auTestButton`) back into `root` and rebuild every cross-
    /// view constraint that the re-parent invalidated. UIKit auto-
    /// removes any constraint whose endpoint views temporarily lack
    /// a common ancestor, and a `removeFromSuperview` puts the view
    /// in a no-superview state mid-move - which kills not just the
    /// chrome's own root anchors but also `separator.top →
    /// topBar.bottom` and the `previewHost.bottom → auBtn.top`
    /// entry inside `previewHostPortraitConstraints`. Re-activating
    /// the cached arrays would silently no-op on the orphaned
    /// references; this method rebuilds them fresh and updates the
    /// cached ivars so the next deactivate/activate cycle is sound.
    ///
    /// Only called on landscape → portrait transitions (see the
    /// `previousLandscape == true` gate in `applyOrientationLayout`).
    /// The initial portrait pass leaves chrome where the launch
    /// code put it - calling this method there would re-parent
    /// already-correctly-parented views and break the still-active
    /// initial constraints.
    func placeChromeInRoot() {
        guard let root = self.rootVC?.view,
              let previewHost = self.previewHost else { return }
        let g = root.safeAreaLayoutGuide

        // Drop the previous round of chrome root constraints
        // explicitly. Some are already auto-removed (those whose
        // endpoint chrome view sat in the sidebar subtree of root,
        // still satisfied the common-ancestor test); others may
        // still be active. Deactivate everything before rebuilding
        // so the new set is the only active set.
        NSLayoutConstraint.deactivate(self.chromeRootConstraints)
        self.chromeRootConstraints.removeAll(keepingCapacity: true)
        self.separatorTopConstraint?.isActive = false

        if let topBar = self.topBar {
            topBar.removeFromSuperview()
            root.addSubview(topBar)
            self.chromeRootConstraints.append(contentsOf: [
                topBar.topAnchor.constraint(equalTo: g.topAnchor, constant: 8),
                topBar.leadingAnchor.constraint(equalTo: g.leadingAnchor, constant: 16),
                topBar.trailingAnchor.constraint(equalTo: g.trailingAnchor, constant: -16),
            ])
        }
        if let statusLabel = self.auStatusLabel {
            statusLabel.removeFromSuperview()
            root.addSubview(statusLabel)
            self.chromeRootConstraints.append(contentsOf: [
                statusLabel.bottomAnchor.constraint(equalTo: g.bottomAnchor, constant: -12),
                statusLabel.leadingAnchor.constraint(equalTo: g.leadingAnchor, constant: 16),
                statusLabel.trailingAnchor.constraint(equalTo: g.trailingAnchor, constant: -16),
            ])
        }
        if let auBtn = self.auTestButton, let statusLabel = self.auStatusLabel {
            auBtn.removeFromSuperview()
            root.addSubview(auBtn)
            self.chromeRootConstraints.append(contentsOf: [
                auBtn.bottomAnchor.constraint(equalTo: statusLabel.topAnchor, constant: -8),
                auBtn.leadingAnchor.constraint(equalTo: g.leadingAnchor, constant: 16),
                auBtn.trailingAnchor.constraint(equalTo: g.trailingAnchor, constant: -16),
            ])
        }

        // Separator stays in root the whole time, but its top is
        // tied to topBar.bottom - the cross-view constraint was
        // auto-removed when topBar left root (above). Build a
        // fresh one against the now re-parented topBar.
        if let separator = self.separator, let topBar = self.topBar {
            let newTop = separator.topAnchor.constraint(equalTo: topBar.bottomAnchor, constant: 10)
            self.separatorTopConstraint = newTop
            self.chromeRootConstraints.append(newTop)
        }

        // `previewHostPortraitConstraints[1]` was
        // previewHost.bottom → auBtn.top. auBtn's brief no-
        // superview state during the move-back invalidated that
        // entry, so the cached array no longer fully anchors
        // previewHost. Build the entire set fresh; the caller
        // re-activates it (we just deactivated the stale version
        // here so the next `activate(self.previewHostPortrait…)`
        // sees the new array).
        if let separator = self.separator, let auBtn = self.auTestButton {
            NSLayoutConstraint.deactivate(self.previewHostPortraitConstraints)
            self.previewHostPortraitConstraints = [
                previewHost.topAnchor.constraint(equalTo: separator.bottomAnchor, constant: 8),
                previewHost.bottomAnchor.constraint(equalTo: auBtn.topAnchor, constant: -16),
                previewHost.leadingAnchor.constraint(equalTo: g.leadingAnchor, constant: 16),
                previewHost.trailingAnchor.constraint(equalTo: g.trailingAnchor, constant: -16),
            ]
        }

        NSLayoutConstraint.activate(self.chromeRootConstraints)
    }

    /// Move the chrome into the sidebar's vertical stack. The
    /// stack manages each child's positioning internally - no
    /// explicit constraints; `removeFromSuperview` drops the
    /// previous root-side constraints automatically (they lose
    /// their common-ancestor with the chrome view). Idempotent -
    /// safe to call on every landscape entry, including after
    /// `placeChromeInRoot` moved the views back.
    ///
    /// Stack order top-to-bottom: title bar, Play button, spacer
    /// (expands to fill), status label. The button sits high so
    /// the trailing-side dynamic island that overlays the sidebar
    /// in landscape-left orientation can't clip it; the status
    /// label drops to the bottom near the home indicator, where
    /// it's least visually noisy.
    func placeChromeInSidebar() {
        guard let stack = self.chromeStack else { return }
        if let topBar = self.topBar {
            topBar.removeFromSuperview()
            topBar.isHidden = false
            stack.addArrangedSubview(topBar)
        }
        if let btn = self.auTestButton {
            btn.removeFromSuperview()
            btn.isHidden = false
            stack.addArrangedSubview(btn)
            // The `>= 50` height anchor from the initial setup is
            // intrinsic (no view-to-view ancestor requirement), so
            // it survives `removeFromSuperview` and re-parenting.
            // Adding another one per rotation would pile redundant
            // constraints onto the same anchor.
        }
        // Flexible spacer: built lazily on first landscape entry,
        // re-used on every subsequent entry (stored on the
        // delegate so the stack doesn't accumulate stray spacers
        // across portrait → landscape cycles). Low content-hugging
        // priority lets UIStackView's distribution=.fill stretch
        // it to absorb whatever vertical room the chrome doesn't
        // need, parking the status label at the bottom.
        let spacer: UIView
        if let existing = self.sidebarChromeSpacer {
            spacer = existing
            spacer.removeFromSuperview()
        } else {
            let s = UIView()
            s.translatesAutoresizingMaskIntoConstraints = false
            s.setContentHuggingPriority(.defaultLow - 1, for: .vertical)
            self.sidebarChromeSpacer = s
            spacer = s
        }
        stack.addArrangedSubview(spacer)
        if let status = self.auStatusLabel {
            status.removeFromSuperview()
            status.isHidden = false
            stack.addArrangedSubview(status)
        }
    }

    /// Apply the uniform scale-to-fit transform to the editor
    /// container so the rasterised bitmap fills `previewHost.bounds`
    /// without distorting aspect ratio. Up-scales as well as down-
    /// scales - small desktop editors expand to fill iPad real
    /// estate; large editors shrink to fit iPhone. No-op when
    /// `scaleEditorToFit` is false.
    func applyEditorScale() {
        guard self.scaleEditorToFit,
              let container = self.editorContainer,
              let host = container.superview else { return }
        let avail = host.bounds.size
        let natural = self.editorNaturalSize
        guard natural.width > 0, natural.height > 0,
              avail.width > 0, avail.height > 0 else { return }
        let s = min(avail.width / natural.width,
                    avail.height / natural.height)
        container.transform = CGAffineTransform(scaleX: s, y: s)
    }

    /// Allocate the sidebar overlay + tap-catcher on first
    /// landscape entry. Held thereafter for the app's lifetime
    /// so subsequent rotations just toggle visibility.
    func buildSidebarIfNeeded() {
        if self.sidebarOverlay != nil { return }
        guard let root = self.rootVC?.view else { return }

        // Tap-catcher: full-screen invisible view behind the
        // sidebar that dismisses on tap-outside. Tapping the
        // sidebar itself doesn't fall through because the
        // sidebar's own gesture absorbs the touch.
        let tap = UIView()
        tap.translatesAutoresizingMaskIntoConstraints = false
        tap.backgroundColor = UIColor(white: 0.0, alpha: 0.0)
        tap.isHidden = true
        root.addSubview(tap)
        NSLayoutConstraint.activate([
            tap.topAnchor.constraint(equalTo: root.topAnchor),
            tap.bottomAnchor.constraint(equalTo: root.bottomAnchor),
            tap.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            tap.trailingAnchor.constraint(equalTo: root.trailingAnchor),
        ])
        let tapGesture = UITapGestureRecognizer(
            target: self, action: #selector(self.tapCatcherTapped))
        tap.addGestureRecognizer(tapGesture)
        self.sidebarTapCatcher = tap

        let sidebar = UIView()
        sidebar.translatesAutoresizingMaskIntoConstraints = false
        sidebar.backgroundColor = .systemBackground
        sidebar.layer.shadowColor = UIColor.black.cgColor
        sidebar.layer.shadowOpacity = 0.3
        sidebar.layer.shadowRadius = 8
        sidebar.layer.shadowOffset = CGSize(width: -2, height: 0)
        root.addSubview(sidebar)
        let g = root.safeAreaLayoutGuide
        let width = min(320.0, root.bounds.width * 0.65)
        // Trailing constraint constant = `width` parks the sidebar
        // off-screen to the right. Animate to 0 on toggle to slide
        // it in over the editor.
        let trailing = sidebar.trailingAnchor.constraint(
            equalTo: root.trailingAnchor, constant: width)
        NSLayoutConstraint.activate([
            sidebar.topAnchor.constraint(equalTo: g.topAnchor),
            sidebar.bottomAnchor.constraint(equalTo: g.bottomAnchor),
            sidebar.widthAnchor.constraint(equalToConstant: width),
            trailing,
        ])
        self.sidebarOverlay = sidebar
        self.sidebarTrailingConstraint = trailing

        // Build the empty chrome stack that the chrome views land
        // in while landscape is active. The actual re-parenting
        // happens in `placeChromeInSidebar`, called from
        // `applyOrientationLayout` on every landscape entry - that
        // method is also the only one that knows how to UNDO the
        // move (`placeChromeInRoot`), so keeping the chrome views
        // in root at build time means we never end up with chrome
        // permanently trapped in an off-screen sidebar.
        let chromeStack = UIStackView()
        chromeStack.translatesAutoresizingMaskIntoConstraints = false
        chromeStack.axis = .vertical
        chromeStack.spacing = 16
        chromeStack.alignment = .fill
        sidebar.addSubview(chromeStack)
        NSLayoutConstraint.activate([
            chromeStack.topAnchor.constraint(equalTo: sidebar.topAnchor, constant: 16),
            chromeStack.leadingAnchor.constraint(equalTo: sidebar.leadingAnchor, constant: 16),
            chromeStack.trailingAnchor.constraint(equalTo: sidebar.trailingAnchor, constant: -16),
        ])
        self.chromeStack = chromeStack
    }

    /// Slide the sidebar in / out. No-op if the sidebar hasn't
    /// been built yet (portrait-only sessions never call
    /// `buildSidebarIfNeeded`).
    func toggleSidebar() {
        if self.sidebarVisible { self.hideSidebar(animated: true) }
        else { self.showSidebar() }
    }

    func showSidebar() {
        guard let trailing = self.sidebarTrailingConstraint else { return }
        self.sidebarTapCatcher?.isHidden = false
        trailing.constant = 0
        UIView.animate(withDuration: 0.25) {
            self.rootVC?.view.layoutIfNeeded()
        }
        self.sidebarVisible = true
    }

    func hideSidebar(animated: Bool) {
        guard let trailing = self.sidebarTrailingConstraint,
              let sidebar = self.sidebarOverlay else { return }
        trailing.constant = sidebar.bounds.width
        let finish: () -> Void = {
            self.sidebarTapCatcher?.isHidden = true
        }
        if animated {
            UIView.animate(withDuration: 0.25, animations: {
                self.rootVC?.view.layoutIfNeeded()
            }, completion: { _ in finish() })
        } else {
            self.rootVC?.view.layoutIfNeeded()
            finish()
        }
        self.sidebarVisible = false
    }

    @objc func tapCatcherTapped() {
        self.hideSidebar(animated: true)
    }
}
