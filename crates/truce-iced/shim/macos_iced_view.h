/**
 * macOS platform layer for truce-iced GUI embedding.
 *
 * Creates a child NSView with wantsLayer=YES. The iced/wgpu compositor
 * handles Metal layer creation and GPU rendering.
 */

#ifndef TRUCE_MACOS_ICED_VIEW_H
#define TRUCE_MACOS_ICED_VIEW_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/**
 * Callbacks from the iced platform view into Rust.
 */
typedef struct {
    /** Called once after view is added to the parent. Passes the NSView* for wgpu surface creation. */
    void (*setup)(void *ctx, void *ns_view);

    /** Called on timer tick (~60fps). Rust drives iced update+render cycle. */
    void (*render)(void *ctx);

    /** Mouse button pressed at (x, y) in pixel coordinates (top-left origin). */
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
} TruceIcedViewCallbacks;

/**
 * Create a child NSView and add it to the parent.
 * If no_timer is non-zero, the repaint timer is NOT started — the host
 * must call truce_iced_view_tick() from its idle callback instead.
 * Returns an opaque handle.
 */
void *truce_iced_view_create(
    void *parent,
    uint32_t width,
    uint32_t height,
    void *ctx,
    const TruceIcedViewCallbacks *callbacks,
    int no_timer
);

/**
 * Drive one render tick. Only needed when no_timer was set in create().
 */
void truce_iced_view_tick(void *view_handle);

/**
 * Destroy the iced view and stop the timer.
 */
void truce_iced_view_destroy(void *view_handle);

#ifdef __cplusplus
}
#endif

#endif /* TRUCE_MACOS_ICED_VIEW_H */
