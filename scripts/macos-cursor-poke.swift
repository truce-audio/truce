// Move the cursor across the main-display center (where a centered editor
// window sits) to drive AppKit cursor-update routing -> NSView hitTest:.
// Used by gui-window-test.sh. CGWarpMouseCursorPosition needs no special
// permission; posted mouseMoved events are best-effort (may need
// Accessibility/Input-Monitoring, which CI runners normally grant).
import CoreGraphics
import Foundation

let b = CGDisplayBounds(CGMainDisplayID())
for i in 0..<40 {
    let p = CGPoint(x: b.midX + CGFloat((i % 11) - 5) * 6,
                    y: b.midY + CGFloat((i % 7) - 3) * 6)
    CGWarpMouseCursorPosition(p)
    CGEvent(mouseEventSource: nil, mouseType: .mouseMoved,
            mouseCursorPosition: p, mouseButton: .left)?.post(tap: .cghidEventTap)
    usleep(40_000)
}
