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

// Liveness registry in au_v2_shim.c (same dylib).
extern uint64_t truce_au_v2_ctx_token(void *ctx);
extern int truce_au_v2_ctx_alive(void *ctx, uint64_t token);

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
#ifndef TRUCE_AU_FIXED_CONTAINER_NAME
#define TRUCE_AU_FIXED_CONTAINER_NAME TruceAuFixedContainer
#endif

/// Fixed-size container the host parents the editor into. AU v2
/// has no standardised host-driven resize protocol, and the major
/// hosts (Logic, REAPER, Ableton, Cubase) each interpret view
/// sizing slightly differently. We sidestep the whole mess by
/// pinning the container to the editor's natural size from
/// `gui_get_size` and ignoring any attempt by the host to resize
/// us. Use AU v3 (or CLAP / VST3 / LV2) for resizable editors.
@interface TRUCE_AU_FIXED_CONTAINER_NAME : NSView
@property(nonatomic, assign) void *rustCtx;
/// Registration token of `rustCtx` (see `truce_au_v2_ctx_token`).
@property(nonatomic, assign) uint64_t ctxToken;
@property(nonatomic, assign) const AuCallbacks *callbacks;
@property(nonatomic, strong) NSTimer *idleTimer;
@end

@implementation TRUCE_AU_FIXED_CONTAINER_NAME
// Whether `rustCtx` is still the live instance this view was made for.
// `au_v2_close` frees the instance without telling the view, and a host
// may dispose the AU before removing (or while still showing) its view;
// the view outlives it, so every call into Rust checks first. Once dead,
// drop the pointer and stop the idle timer for good.
- (BOOL)ctxLive {
    if (self.rustCtx == NULL || self.callbacks == NULL) return NO;
    if (truce_au_v2_ctx_alive(self.rustCtx, self.ctxToken)) return YES;
    self.rustCtx = NULL;
    [self.idleTimer invalidate];
    self.idleTimer = nil;
    return NO;
}

- (void)setFrameSize:(NSSize)newSize {
    // Pin to the editor's natural size. Any host call to resize
    // us (Logic embedding into its plug-in pane, Ableton's frame
    // measurement, REAPER's FX panel layout) is ignored.
    if ([self ctxLive]) {
        uint32_t natW = 0, natH = 0;
        self.callbacks->gui_get_size(self.rustCtx, &natW, &natH);
        if (natW > 0 && natH > 0) {
            [super setFrameSize:NSMakeSize((CGFloat)natW, (CGFloat)natH)];
            return;
        }
    }
    [super setFrameSize:newSize];
}

// Drive `Editor::idle` ~60fps on the UI thread while the view is on screen
// (started when it gains a window, stopped when it loses one). Common
// run-loop modes so the tick survives menu / resize tracking. The block
// holds a weak ref, so the timer never keeps the view alive; the view
// owns the timer and invalidates it on removal / dealloc.
- (void)viewDidMoveToWindow {
    [super viewDidMoveToWindow];
    if (self.window) {
        // `gui_idle` is a v6 (append-only) callback; guard the pointer in
        // case a host ever pairs this view with an older core build. An
        // editor whose `idle` no-ops (v8 `gui_wants_idle` == 0: baseview
        // repaints it) gets no 60 Hz timer at all.
        BOOL wantsIdle = YES;
        uint32_t abi = self.callbacks->abi_version;
        if ((abi & TRUCE_AU_ABI_MAGIC_MASK) == TRUCE_AU_ABI_MAGIC && (abi & 0xFFu) >= 8u &&
            self.callbacks->gui_wants_idle && [self ctxLive]) {
            wantsIdle = self.callbacks->gui_wants_idle(self.rustCtx) != 0;
        }
        if (!self.idleTimer && wantsIdle && [self ctxLive] && self.callbacks->gui_idle) {
            __weak TRUCE_AU_FIXED_CONTAINER_NAME *weakSelf = self;
            NSTimer *t = [NSTimer timerWithTimeInterval:1.0 / 60.0
                                                repeats:YES
                                                  block:^(NSTimer *timer) {
                                                    TRUCE_AU_FIXED_CONTAINER_NAME *s = weakSelf;
                                                    if (!s) {
                                                        [timer invalidate];
                                                        return;
                                                    }
                                                    if ([s ctxLive] &&
                                                        s.callbacks->gui_idle) {
                                                        s.callbacks->gui_idle(s.rustCtx);
                                                    }
                                                  }];
            [[NSRunLoop mainRunLoop] addTimer:t forMode:NSRunLoopCommonModes];
            self.idleTimer = t;
        }
    } else {
        [self.idleTimer invalidate];
        self.idleTimer = nil;
    }
}

- (void)dealloc {
    [self.idleTimer invalidate];
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
    TRUCE_AU_FIXED_CONTAINER_NAME *container =
        [[TRUCE_AU_FIXED_CONTAINER_NAME alloc] initWithFrame:frame];
    container.rustCtx = ctx;
    container.ctxToken = truce_au_v2_ctx_token(ctx);
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
