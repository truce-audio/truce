/**
 * AU v2 CocoaUI view factory.
 *
 * Implements the AUCocoaUIBase protocol — the host calls
 * uiViewForAudioUnit:withSize: to get our NSView.
 */

@import AppKit;
@import AudioToolbox;
#import <AudioUnit/AUCocoaUIView.h>

#include "au_shim_types.h"

// Private property to retrieve the Rust context from an AudioUnit.
#define kTrucePrivateProperty_RustContext 64000

// Globals from au_shim_common.c
extern const AuCallbacks *g_callbacks;

// Dynamic class name to avoid collisions between plugins.
// Set via -DTRUCE_AU_VIEW_FACTORY_NAME in build.rs.
#ifndef TRUCE_AU_VIEW_FACTORY_NAME
#define TRUCE_AU_VIEW_FACTORY_NAME TruceAUViewFactory
#endif

@interface TRUCE_AU_VIEW_FACTORY_NAME : NSObject <AUCocoaUIBase>
@end

@implementation TRUCE_AU_VIEW_FACTORY_NAME

- (unsigned)interfaceVersion {
    return 0;
}

- (NSView *)uiViewForAudioUnit:(AudioUnit)au withSize:(NSSize)preferredSize {
    if (!g_callbacks) return nil;

    // Get the Rust context via our private property
    void *ctx = NULL;
    UInt32 sz = sizeof(ctx);
    OSStatus err = AudioUnitGetProperty(au, kTrucePrivateProperty_RustContext,
        kAudioUnitScope_Global, 0, &ctx, &sz);
    if (err != noErr || !ctx) return nil;

    // Check if the plugin has an editor
    if (!g_callbacks->gui_has_editor(ctx)) return nil;

    // Get editor size
    uint32_t w = 0, h = 0;
    g_callbacks->gui_get_size(ctx, &w, &h);
    if (w == 0 || h == 0) return nil;

    // w/h are in logical points — use directly.
    NSRect frame = NSMakeRect(0, 0, w, h);
    NSView *container = [[NSView alloc] initWithFrame:frame];

    // Open the editor — creates the truce platform view as a child
    g_callbacks->gui_open(ctx, (__bridge void *)container);

    return container;
}

@end

// Return the class name as a C string for the v2 shim's CocoaUI property.
#define _TRUCE_STRINGIFY(x) #x
#define TRUCE_STRINGIFY(x) _TRUCE_STRINGIFY(x)

const char *truce_au_view_factory_class_name(void) {
    return TRUCE_STRINGIFY(TRUCE_AU_VIEW_FACTORY_NAME);
}
