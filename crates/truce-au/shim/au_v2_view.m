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
    [super setFrameSize:newSize];
    if (!self.canResize || self.rustCtx == NULL || self.callbacks == NULL) return;
    uint32_t w = (uint32_t)MAX(1.0, round(newSize.width));
    uint32_t h = (uint32_t)MAX(1.0, round(newSize.height));
    self.callbacks->gui_set_size(self.rustCtx, w, h);
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
        // Match the host's current size immediately - it may have
        // resized the superview before we installed the observer.
        [self resizeToSuperview];
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

    // AU v2 hosts may drive resize by re-calling this method with
    // a new `preferredSize` and discarding the old view (the
    // `setFrameSize:` / superview-observer path covers hosts that
    // resize the existing view in-place). Honour `preferredSize`
    // when the editor opted in; otherwise stick with the editor's
    // natural size.
    //
    // Inform the editor of the new size BEFORE `gui_open` so the
    // newly-opened editor starts at the requested dimensions -
    // otherwise the first frame paints at the old size and only
    // catches up on the next `on_frame` tick.
    uint32_t w = 0, h = 0;
    if (canResize && preferredSize.width > 0 && preferredSize.height > 0) {
        w = (uint32_t)round(preferredSize.width);
        h = (uint32_t)round(preferredSize.height);
        cb->gui_set_size(ctx, w, h);
    } else {
        cb->gui_get_size(ctx, &w, &h);
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
