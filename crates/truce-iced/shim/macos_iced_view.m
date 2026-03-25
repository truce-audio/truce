/**
 * macOS platform layer for truce-iced GUI embedding.
 *
 * Creates a child NSView with wantsLayer=YES. The iced/wgpu runtime
 * handles Metal layer creation and GPU rendering — this layer just
 * drives the timer and forwards mouse events.
 */

@import AppKit;
@import QuartzCore;
@import Metal;

#include "macos_iced_view.h"
#include <stdio.h>

#ifndef TRUCE_ICED_VIEW_CLASS_NAME
#define TRUCE_ICED_VIEW_CLASS_NAME TruceIcedPluginView
#endif

@interface TRUCE_ICED_VIEW_CLASS_NAME : NSView {
@public
    void *rustCtx;
    TruceIcedViewCallbacks callbacks;
    NSTimer *repaintTimer;
    CAMetalLayer *metalLayer;
    uint32_t viewWidth;
    uint32_t viewHeight;
    BOOL isTracking;
}
@end

@implementation TRUCE_ICED_VIEW_CLASS_NAME

- (instancetype)initWithFrame:(NSRect)frame
                       rustCtx:(void *)ctx
                     callbacks:(const TruceIcedViewCallbacks *)cbs
                         width:(uint32_t)w
                        height:(uint32_t)h {
    self = [super initWithFrame:frame];
    if (self) {
        rustCtx = ctx;
        callbacks = *cbs;
        viewWidth = w;
        viewHeight = h;
        isTracking = NO;

        // Create Metal layer explicitly (proven approach from truce-gpu)
        self.wantsLayer = YES;
        metalLayer = [CAMetalLayer layer];
        metalLayer.device = MTLCreateSystemDefaultDevice();
        metalLayer.pixelFormat = MTLPixelFormatBGRA8Unorm;
        metalLayer.framebufferOnly = YES;
        metalLayer.presentsWithTransaction = YES;
        metalLayer.contentsScale = self.window
            ? self.window.backingScaleFactor
            : [NSScreen mainScreen].backingScaleFactor;
        // Don't set drawableSize here — let wgpu's surface.configure()
        // set it from the Rust side so it matches the viewport exactly.
        self.layer = metalLayer;
    }
    return self;
}

- (void)startRunloop {
    // Start repaint timer at ~60fps
    repaintTimer = [NSTimer scheduledTimerWithTimeInterval:1.0/60.0
                                                   target:self
                                                 selector:@selector(repaintTick:)
                                                 userInfo:nil
                                                  repeats:YES];
}

- (void)dealloc {
    [repaintTimer invalidate];
    repaintTimer = nil;
}

- (BOOL)isFlipped {
    return YES;
}

- (BOOL)acceptsFirstMouse:(NSEvent *)event {
    return YES;
}

- (BOOL)acceptsFirstResponder {
    return YES;
}

// ---------------------------------------------------------------------------
// Repaint — trigger Rust-side iced update+render
// ---------------------------------------------------------------------------

- (void)repaintTick:(NSTimer *)timer {
    if (callbacks.render) {
        callbacks.render(rustCtx);
    }
}

// ---------------------------------------------------------------------------
// Mouse events
// ---------------------------------------------------------------------------

- (NSPoint)eventPoint:(NSEvent *)event {
    // Return logical points — iced works in points, not pixels.
    return [self convertPoint:event.locationInWindow fromView:nil];
}

- (void)mouseDown:(NSEvent *)event {
    NSPoint p = [self eventPoint:event];
    if (event.clickCount >= 2 && callbacks.double_click) {
        callbacks.double_click(rustCtx, (float)p.x, (float)p.y);
        return;
    }
    if (!callbacks.mouse_down) return;
    callbacks.mouse_down(rustCtx, (float)p.x, (float)p.y);
    isTracking = YES;
}

- (void)mouseDragged:(NSEvent *)event {
    if (!callbacks.mouse_dragged || !isTracking) return;
    NSPoint p = [self eventPoint:event];
    callbacks.mouse_dragged(rustCtx, (float)p.x, (float)p.y);
}

- (void)mouseUp:(NSEvent *)event {
    if (!callbacks.mouse_up) return;
    NSPoint p = [self eventPoint:event];
    callbacks.mouse_up(rustCtx, (float)p.x, (float)p.y);
    isTracking = NO;
}

- (void)scrollWheel:(NSEvent *)event {
    if (!callbacks.scroll) return;
    NSPoint p = [self eventPoint:event];
    float dy = (float)event.scrollingDeltaY;
    if (!event.hasPreciseScrollingDeltas) {
        dy *= 10.0f;
    }
    callbacks.scroll(rustCtx, (float)p.x, (float)p.y, dy);
}

- (void)mouseMoved:(NSEvent *)event {
    if (!callbacks.mouse_moved) return;
    NSPoint p = [self eventPoint:event];
    uint8_t over_widget = callbacks.mouse_moved(rustCtx, (float)p.x, (float)p.y);
    if (over_widget) {
        [[NSCursor pointingHandCursor] set];
    } else {
        [[NSCursor arrowCursor] set];
    }
}

- (void)mouseExited:(NSEvent *)event {
    (void)event;
    if (callbacks.mouse_moved)
        callbacks.mouse_moved(rustCtx, -1.0f, -1.0f);
    [[NSCursor arrowCursor] set];
}

- (void)updateTrackingAreas {
    [super updateTrackingAreas];
    for (NSTrackingArea *area in self.trackingAreas) {
        [self removeTrackingArea:area];
    }
    NSTrackingArea *ta = [[NSTrackingArea alloc]
        initWithRect:self.bounds
             options:(NSTrackingMouseMoved | NSTrackingMouseEnteredAndExited | NSTrackingActiveAlways)
               owner:self
            userInfo:nil];
    [self addTrackingArea:ta];
}

@end

// ---------------------------------------------------------------------------
// C API
// ---------------------------------------------------------------------------

void *truce_iced_view_create(
    void *parent,
    uint32_t width,
    uint32_t height,
    void *ctx,
    const TruceIcedViewCallbacks *callbacks
) {
    NSView *parentView = (__bridge NSView *)parent;
    if (!parentView) return NULL;

    // width/height are in logical points — use directly.
    NSRect frame = NSMakeRect(0, 0, width, height);

    TRUCE_ICED_VIEW_CLASS_NAME *view = [[TRUCE_ICED_VIEW_CLASS_NAME alloc]
        initWithFrame:frame
              rustCtx:ctx
            callbacks:callbacks
                width:width
               height:height];

    [parentView addSubview:view];

    // Pass the CAMetalLayer (not NSView) for wgpu surface creation
    if (view->callbacks.setup) {
        view->callbacks.setup(view->rustCtx, (__bridge void *)view->metalLayer);
    }

    [view startRunloop];

    return (__bridge_retained void *)view;
}

void truce_iced_view_destroy(void *view_handle) {
    if (!view_handle) return;
    TRUCE_ICED_VIEW_CLASS_NAME *view = (__bridge_transfer TRUCE_ICED_VIEW_CLASS_NAME *)view_handle;
    [view->repaintTimer invalidate];
    view->repaintTimer = nil;
    [view removeFromSuperview];
}
