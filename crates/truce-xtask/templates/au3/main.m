// Minimal containing app for AU v3 appex.
// Launching this app registers the embedded audio unit extension with the system.
#import <Cocoa/Cocoa.h>

int main(int argc, const char *argv[]) {
    @autoreleasepool {
        [NSApplication sharedApplication];
        NSAlert *alert = [[NSAlert alloc] init];
        alert.messageText = @"Audio Plugin Installed";
        alert.informativeText = @"The audio unit extension has been registered with the system. "
                                 "You can close this app and use the plugin in your DAW.";
        [alert addButtonWithTitle:@"Quit"];
        [alert runModal];
    }
    return 0;
}
