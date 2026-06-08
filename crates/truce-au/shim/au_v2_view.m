/**
 * AU v2 Cocoa UI view factory.
 *
 * Defines the `AUCocoaUIBase` class the host instantiates after
 * reading our `kAudioUnitProperty_CocoaUI`. The class is compiled
 * into every truce plugin dylib so it appears in `__objc_classlist`,
 * which `[NSBundle classNamed:]`-based hosts (REAPER) require.
 *
 * The class name MUST be unique per plugin. AppKit/AudioUnit hosts
 * load every installed `.component` into one process; if two plugins
 * publish a class with the same name, `libobjc` keeps the first one
 * and `[NSBundle classNamed:name]` on the loser's bundle returns nil
 * - the host then thinks the plugin has no GUI. Uniqueness comes
 * from the `TRUCE_AU_PLUGIN_ID` env var that `cargo-truce` sets at
 * build time; the build.rs sanitises and passes it as a `-D` define.
 * Without that env (plain `cargo build` for unit tests), the class
 * falls back to a default name - fine for isolated tests, not for
 * multi-plugin hosting.
 */

@import AppKit;
@import AudioToolbox;
#import <AudioUnit/AUCocoaUIView.h>

#include "au_shim_types.h"

// Private properties exposed by `au_v2_shim.c`:
//   64000: AuPlugin context pointer (rustCtx)
//   64001: pointer to the AU's AuCallbacks table (g_callbacks of the
//          dylib that owns this AudioUnit). Reading both via the AU
//          dispatch table keeps the methods plugin-agnostic - per-
//          dylib globals reached are always the right ones.
#define kTrucePrivateProperty_RustContext  64000
#define kTrucePrivateProperty_AuCallbacks  64001

#ifndef TRUCE_AU_VIEW_FACTORY_NAME
// Default name when `TRUCE_AU_PLUGIN_ID` is unset - keeps `cargo build`
// of the workspace cdylibs working for unit tests.
#define TRUCE_AU_VIEW_FACTORY_NAME TruceAUCocoaViewProxy
#endif

/// Resizable container the host parents the editor into. The
/// host (Logic, REAPER's AU support, AUM via AUv3 bridge) resizes
/// our outer view by calling `setFrameSize:` directly when its
/// own bounds change. AppKit doesn't fire any plugin-friendly
/// notification on that path, so we override here and forward to
/// the editor's `gui_set_size`. Plugins whose editor opted out of
/// resize get an `autoresizingMask = NSViewNotSizable` and `setFrameSize:`
/// is a no-op beyond AppKit's default - the host's outer
/// container will stretch but our view stays at its original size.
@interface TruceAuResizableContainer : NSView
@property(nonatomic, assign) void *rustCtx;
@property(nonatomic, assign) const AuCallbacks *callbacks;
@property(nonatomic, assign) BOOL canResize;
@property(nonatomic, weak) NSView *observedSuperview;
@end

@implementation TruceAuResizableContainer
- (void)setFrameSize:(NSSize)newSize {
    // Non-resizable editor: pin the container to the editor's
    // natural size so a host that calls `setFrameSize:` with its
    // own default (e.g. Ableton's macOS AU v2 host) can't shrink
    // the container below the editor child it carries.
    if (!self.canResize) {
        if (self.rustCtx != NULL && self.callbacks != NULL) {
            uint32_t natW = 0, natH = 0;
            self.callbacks->gui_get_size(self.rustCtx, &natW, &natH);
            if (natW > 0 && natH > 0) {
                [super setFrameSize:NSMakeSize((CGFloat)natW, (CGFloat)natH)];
                return;
            }
        }
        [super setFrameSize:newSize];
        return;
    }
    // Resizable editor: super-call only. Don't propagate
    // `newSize` to `gui_set_size` - AU v2 has no standardised
    // host-driven resize API, so Logic (and other hosts) call
    // `setFrameSize:` here to position the embedded view into
    // their plug-in pane (whatever size that happens to be), NOT
    // to request the plug-in resize. Treating it as a resize
    // request snaps the editor to its `max_size` because the host
    // pane is typically much bigger than the editor's natural
    // size. The editor stays at the size it reported via
    // `gui_get_size`; the host's pane shows it at that size (the
    // user may see empty space around it depending on the host's
    // layout).
    [super setFrameSize:newSize];
}

/// REAPER and Logic resize the plugin's outer window but never
/// call `setFrameSize:` on the embedded AU view (the AU v2
/// contract historically treated the view as fixed-size). Observe
/// the superview's frame instead and follow the host's outer
/// dimensions manually when the editor opts in.
/// `viewDidMoveToSuperview` is the canonical hook for installing
/// / uninstalling the observer.
- (void)viewDidMoveToSuperview {
    [super viewDidMoveToSuperview];
    if (self.observedSuperview) {
        [self.observedSuperview setPostsFrameChangedNotifications:NO];
        [[NSNotificationCenter defaultCenter]
            removeObserver:self
                      name:NSViewFrameDidChangeNotification
                    object:self.observedSuperview];
        self.observedSuperview = nil;
    }
    if (self.canResize && self.superview) {
        self.observedSuperview = self.superview;
        [self.superview setPostsFrameChangedNotifications:YES];
        [[NSNotificationCenter defaultCenter]
            addObserver:self
               selector:@selector(superviewFrameDidChange:)
                   name:NSViewFrameDidChangeNotification
                 object:self.superview];
        // No initial `resizeToSuperview` sync. Logic Pro's AU v2
        // host parents us into a plug-in pane that's much bigger
        // than the editor's natural size; immediately filling it
        // snaps the editor to its `max_size` instead of opening at
        // the natural size the plug-in author intended. The
        // observer above still picks up *future* superview frame
        // changes (user-drag resize in hosts that propagate it),
        // and our `setFrameSize:` override still services hosts
        // that drive resize through the embedded view directly.
    }
}

- (void)superviewFrameDidChange:(NSNotification *)note {
    [self resizeToSuperview];
}

- (void)resizeToSuperview {
    if (!self.superview) return;
    NSSize target = self.superview.bounds.size;
    if (target.width <= 0 || target.height <= 0) return;
    if (NSEqualSizes(target, self.frame.size)) return;
    [self setFrameSize:target];
}

- (void)dealloc {
    if (self.observedSuperview) {
        [self.observedSuperview setPostsFrameChangedNotifications:NO];
        [[NSNotificationCenter defaultCenter]
            removeObserver:self
                      name:NSViewFrameDidChangeNotification
                    object:self.observedSuperview];
    }
}
@end

@interface TRUCE_AU_VIEW_FACTORY_NAME : NSObject <AUCocoaUIBase>
@end

@implementation TRUCE_AU_VIEW_FACTORY_NAME

- (unsigned)interfaceVersion {
    return 0;
}

- (NSView *)uiViewForAudioUnit:(AudioUnit)au withSize:(NSSize)preferredSize {
    void *ctx = NULL;
    UInt32 ctxSize = sizeof(ctx);
    if (AudioUnitGetProperty(au, kTrucePrivateProperty_RustContext,
            kAudioUnitScope_Global, 0, &ctx, &ctxSize) != noErr || !ctx) {
        return nil;
    }

    const AuCallbacks *cb = NULL;
    UInt32 cbSize = sizeof(cb);
    if (AudioUnitGetProperty(au, kTrucePrivateProperty_AuCallbacks,
            kAudioUnitScope_Global, 0, &cb, &cbSize) != noErr || !cb) {
        return nil;
    }

    if (!cb->gui_has_editor(ctx)) return nil;

    BOOL canResize = cb->gui_can_resize(ctx) != 0;

    // `preferredSize` per Apple's AUCocoaUIView spec is *the
    // maximum the host can accommodate*, not the desired size.
    // Logic Pro passes its full available plug-in pane (1000+
    // pixels) here; treating it as a target snaps the editor
    // straight to host-max instead of opening at the plug-in's
    // natural size. Read our natural size from `gui_get_size`
    // first, then only push `gui_set_size` when the natural is
    // *bigger* than the host's max (i.e. to shrink, not grow).
    uint32_t w = 0, h = 0;
    cb->gui_get_size(ctx, &w, &h);
    if (canResize && preferredSize.width > 0 && preferredSize.height > 0) {
        uint32_t maxW = (uint32_t)round(preferredSize.width);
        uint32_t maxH = (uint32_t)round(preferredSize.height);
        if (w > maxW || h > maxH) {
            uint32_t reqW = w > maxW ? maxW : w;
            uint32_t reqH = h > maxH ? maxH : h;
            cb->gui_set_size(ctx, reqW, reqH);
            cb->gui_get_size(ctx, &w, &h);
        }
    }
    if (w == 0 || h == 0) return nil;

    NSRect frame = NSMakeRect(0, 0, w, h);
    TruceAuResizableContainer *container =
        [[TruceAuResizableContainer alloc] initWithFrame:frame];
    container.rustCtx = ctx;
    container.callbacks = cb;
    container.canResize = canResize;
    // Belt-and-braces: autoresizingMask catches hosts that do
    // drive resize via `setFrameSize:` on the existing view, while
    // the superview-frame observer (installed in
    // `viewDidMoveToSuperview`) catches hosts that resize the
    // outer container without notifying the embedded view.
    if (canResize) {
        container.autoresizingMask = NSViewWidthSizable | NSViewHeightSizable;
    }
    cb->gui_open(ctx, (__bridge void *)container);
    return container;
}

@end

// Stringify the class name for the v2 shim's `kAudioUnitProperty_CocoaUI`
// response. Two-step macro so the argument is expanded before stringification.
#define _TRUCE_STRINGIFY(x) #x
#define TRUCE_STRINGIFY(x) _TRUCE_STRINGIFY(x)

const char *truce_au_view_factory_class_name(void) {
    return TRUCE_STRINGIFY(TRUCE_AU_VIEW_FACTORY_NAME);
}
