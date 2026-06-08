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

/// Fixed-size container the host parents the editor into. AU v2
/// has no standardised host-driven resize protocol, and the major
/// hosts (Logic, REAPER, Ableton, Cubase) each interpret view
/// sizing slightly differently. We sidestep the whole mess by
/// pinning the container to the editor's natural size from
/// `gui_get_size` and ignoring any attempt by the host to resize
/// us. Use AU v3 (or CLAP / VST3 / LV2) for resizable editors.
@interface TruceAuFixedContainer : NSView
@property(nonatomic, assign) void *rustCtx;
@property(nonatomic, assign) const AuCallbacks *callbacks;
@end

@implementation TruceAuFixedContainer
- (void)setFrameSize:(NSSize)newSize {
    // Pin to the editor's natural size. Any host call to resize
    // us (Logic embedding into its plug-in pane, Ableton's frame
    // measurement, REAPER's FX panel layout) is ignored.
    if (self.rustCtx != NULL && self.callbacks != NULL) {
        uint32_t natW = 0, natH = 0;
        self.callbacks->gui_get_size(self.rustCtx, &natW, &natH);
        if (natW > 0 && natH > 0) {
            [super setFrameSize:NSMakeSize((CGFloat)natW, (CGFloat)natH)];
            return;
        }
    }
    [super setFrameSize:newSize];
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

    // AU v2 ignores `preferredSize` and any `.resizable(true)` on
    // the editor. Resize-capable editors (CLAP / VST3 / LV2 /
    // AU v3 / standalone) get host-driven resize; AU v2 stays
    // fixed at the editor's natural size for every host.
    uint32_t w = 0, h = 0;
    cb->gui_get_size(ctx, &w, &h);
    if (w == 0 || h == 0) return nil;
    (void)preferredSize;

    NSRect frame = NSMakeRect(0, 0, w, h);
    TruceAuFixedContainer *container =
        [[TruceAuFixedContainer alloc] initWithFrame:frame];
    container.rustCtx = ctx;
    container.callbacks = cb;
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
