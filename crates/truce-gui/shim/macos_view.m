/**
 * macOS platform layer for truce GUI embedding.
 *
 * Creates a child NSView with a CALayer, blits RGBA pixels from the
 * Rust renderer, and dispatches mouse events back to Rust.
 */

@import AppKit;
@import QuartzCore;
@import CoreGraphics;

#include "macos_view.h"
#include <stdio.h>

// ---------------------------------------------------------------------------
// Custom NSView subclass
//
// Use a unique class name per process to avoid ObjC class collisions
// when multiple plugin formats (CLAP, VST3, AU) are loaded together.
// ---------------------------------------------------------------------------

#ifndef TRUCE_VIEW_CLASS_NAME
#define TRUCE_VIEW_CLASS_NAME TrucePluginView
#endif

@interface TRUCE_VIEW_CLASS_NAME : NSView {
@public
    void *rustCtx;
    TruceViewCallbacks callbacks;
    NSTimer *repaintTimer;
    uint32_t viewWidth;
    uint32_t viewHeight;
    BOOL isTracking;
}
@end

@implementation TRUCE_VIEW_CLASS_NAME

- (instancetype)initWithFrame:(NSRect)frame
                       rustCtx:(void *)ctx
                     callbacks:(const TruceViewCallbacks *)cbs
                         width:(uint32_t)w
                        height:(uint32_t)h {
    self = [super initWithFrame:frame];
    if (self) {
        rustCtx = ctx;
        callbacks = *cbs;
        viewWidth = w;
        viewHeight = h;
        isTracking = NO;

        self.wantsLayer = YES;
        self.layer.contentsGravity = kCAGravityTopLeft;
        self.layer.magnificationFilter = kCAFilterNearest;

        // Start repaint timer at ~60fps
        repaintTimer = [NSTimer scheduledTimerWithTimeInterval:1.0/60.0
                                                       target:self
                                                     selector:@selector(repaintTick:)
                                                     userInfo:nil
                                                      repeats:YES];
    }
    return self;
}

- (void)dealloc {
    [repaintTimer invalidate];
    repaintTimer = nil;
}

- (BOOL)isFlipped {
    return YES; // Top-left origin (matches our pixel coordinates)
}

- (BOOL)acceptsFirstMouse:(NSEvent *)event {
    return YES;
}

- (BOOL)acceptsFirstResponder {
    return YES;
}

// ---------------------------------------------------------------------------
// Repaint
// ---------------------------------------------------------------------------

- (void)repaintTick:(NSTimer *)timer {
    if (!callbacks.render) return;

    uint32_t w = 0, h = 0;
    const uint8_t *pixels = callbacks.render(rustCtx, &w, &h);
    if (!pixels || w == 0 || h == 0) return;

    // Create CGImage from RGBA premultiplied pixel data
    CGColorSpaceRef cs = CGColorSpaceCreateDeviceRGB();
    CGDataProviderRef dp = CGDataProviderCreateWithData(
        NULL, pixels, w * h * 4, NULL);

    CGImageRef image = CGImageCreate(
        w, h,
        8,              // bits per component
        32,             // bits per pixel
        w * 4,          // bytes per row
        cs,
        kCGBitmapByteOrderDefault | kCGImageAlphaPremultipliedLast,
        dp,
        NULL,           // decode
        false,          // interpolate
        kCGRenderingIntentDefault);

    if (image) {
        [CATransaction begin];
        [CATransaction setDisableActions:YES];
        self.layer.contents = (__bridge id)image;
        [CATransaction commit];
        CGImageRelease(image);
    }

    CGDataProviderRelease(dp);
    CGColorSpaceRelease(cs);
}

// ---------------------------------------------------------------------------
// Mouse events (coordinates flipped to top-left origin)
// ---------------------------------------------------------------------------

- (NSPoint)eventPoint:(NSEvent *)event {
    NSPoint p = [self convertPoint:event.locationInWindow fromView:nil];
    // Convert from points to pixel coordinates (Retina: 2x)
    CGFloat scale = self.window ? self.window.backingScaleFactor
                                : [NSScreen mainScreen].backingScaleFactor;
    if (scale < 1.0) scale = 1.0;
    p.x *= scale;
    p.y *= scale;
    return p;
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
        dy *= 10.0f; // Line-based scroll → pixel-like
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
        callbacks.mouse_moved(rustCtx, -1.0f, -1.0f); // clear hover
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

void *truce_view_create(
    void *parent,
    uint32_t width,
    uint32_t height,
    void *ctx,
    const TruceViewCallbacks *callbacks
) {
    NSView *parentView = (__bridge NSView *)parent;
    if (!parentView) return NULL;

    // width/height are in logical points — use directly.
    NSRect frame = NSMakeRect(0, 0, width, height);
    TRUCE_VIEW_CLASS_NAME *view = [[TRUCE_VIEW_CLASS_NAME alloc]
        initWithFrame:frame
              rustCtx:ctx
            callbacks:callbacks
                width:width
               height:height];

    [parentView addSubview:view];
    return (__bridge_retained void *)view;
}

void truce_view_destroy(void *view_handle) {
    if (!view_handle) return;
    TRUCE_VIEW_CLASS_NAME *view = (__bridge_transfer TRUCE_VIEW_CLASS_NAME *)view_handle;
    [view->repaintTimer invalidate];
    view->repaintTimer = nil;
    [view removeFromSuperview];
}


/// Return the main screen's backing scale factor (Retina = 2.0, normal = 1.0).
/// Called by all format wrappers to convert pixel sizes to logical points.
__attribute__((visibility("default")))
double truce_platform_backing_scale(void) {
    CGFloat scale = [NSScreen mainScreen].backingScaleFactor;
    return (scale < 1.0) ? 1.0 : (double)scale;
}
