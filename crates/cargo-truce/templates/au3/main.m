// Containing app for the AU v3 appex. When the plugin ships a standalone
// host that host replaces this; otherwise this informational app is the
// container. The appex is the deliverable - this confirms the install and
// helps locate the Audio Unit inside a DAW. Display name and identifier
// codes are substituted per plugin when the project is materialized.
#import <Cocoa/Cocoa.h>

int main(int argc, const char *argv[]) {
    @autoreleasepool {
        [NSApplication sharedApplication];
        [NSApp setActivationPolicy:NSApplicationActivationPolicyRegular];
        [NSApp activateIgnoringOtherApps:YES];

        NSAlert *alert = [[NSAlert alloc] init];
        alert.messageText = @"PLUGIN_DISPLAY_NAME";
        alert.informativeText =
            @"This Audio Unit (AU v3) is installed and available to your DAW.\n\n"
             "Type  AU_TYPE_CODE    Subtype  AU_SUBTYPE_CODE    "
             "Manufacturer  AU_MANUFACTURER_CODE\n\n"
             "Use those four-character codes to find the plugin if your host "
             "lists Audio Units by code. This window is just the installer "
             "container - the plugin runs inside your DAW.";
        [alert addButtonWithTitle:@"Reveal in Finder"];
        [alert addButtonWithTitle:@"Done"];

        NSModalResponse response = [alert runModal];
        if (response == NSAlertFirstButtonReturn) {
            [[NSWorkspace sharedWorkspace]
                activateFileViewerSelectingURLs:@[ NSBundle.mainBundle.bundleURL ]];
        }
    }
    return 0;
}
