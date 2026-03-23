/**
 * macOS platform layer for truce GUI embedding.
 *
 * Creates a child NSView, blits RGBA pixels via CALayer, handles
 * mouse events, and runs a repaint timer.
 */

#ifndef TRUCE_MACOS_VIEW_H
#define TRUCE_MACOS_VIEW_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/**
 * Callbacks from the platform layer into Rust.
 * Set once before creating the view.
 */
typedef struct {
    /** Called on timer tick (~60fps). Render and return pixel data. */
    const uint8_t *(*render)(void *ctx, uint32_t *out_width, uint32_t *out_height);

    /** Mouse button pressed at (x, y) in view coordinates (top-left origin). */
    void (*mouse_down)(void *ctx, float x, float y);

    /** Mouse dragged to (x, y). */
    void (*mouse_dragged)(void *ctx, float x, float y);

    /** Mouse button released at (x, y). */
    void (*mouse_up)(void *ctx, float x, float y);

    /** Scroll wheel delta at (x, y). */
    void (*scroll)(void *ctx, float x, float y, float delta_y);

    /** Double-click at (x, y). */
    void (*double_click)(void *ctx, float x, float y);

    /** Mouse moved to (x, y) — for hover tracking.
     *  Returns 1 if cursor is over a clickable widget, 0 otherwise. */
    uint8_t (*mouse_moved)(void *ctx, float x, float y);
} TruceViewCallbacks;

/**
 * Create a child NSView and add it to the given parent.
 * Returns an opaque handle (TruceNSView*).
 *
 * parent: NSView* from the host
 * width, height: initial size
 * ctx: opaque pointer passed to all callbacks
 * callbacks: function pointers for events and rendering
 */
void *truce_view_create(
    void *parent,
    uint32_t width,
    uint32_t height,
    void *ctx,
    const TruceViewCallbacks *callbacks
);

/**
 * Destroy the view and stop the timer.
 */
void truce_view_destroy(void *view_handle);

#ifdef __cplusplus
}
#endif

#endif /* TRUCE_MACOS_VIEW_H */
