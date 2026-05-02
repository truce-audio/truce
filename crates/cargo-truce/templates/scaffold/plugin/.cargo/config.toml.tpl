# Local build-environment config. Gitignored — set your own values.
# `cargo truce install` and `cargo truce package` both read from here.

[env]
# macOS code signing (see `cargo truce doctor`):
# TRUCE_SIGNING_IDENTITY           = "Developer ID Application: Your Name (TEAMID)"
# TRUCE_INSTALLER_SIGNING_IDENTITY = "Developer ID Installer: Your Name (TEAMID)"

# AAX SDK location (macOS and Windows):
# AAX_SDK_PATH = "/path/to/aax-sdk-2-9-0"
# AAX_SDK_PATH = 'C:\Users\you\aax-sdk-2-9-0'

# macOS notarization (alternative to using a keychain profile):
# APPLE_ID              = "you@example.com"
# TEAM_ID               = "ABCDEFG123"
# APP_SPECIFIC_PASSWORD = "xxxx-xxxx-xxxx-xxxx"

# Windows Authenticode via Azure Trusted Signing:
# AZURE_TENANT_ID     = "..."
# AZURE_CLIENT_ID     = "..."
# AZURE_CLIENT_SECRET = "..."

# Windows .pfx password (when using [windows.signing].pfx_path):
# TRUCE_PFX_PASSWORD = "..."

# Screenshot testing — which OS owns the committed reference PNGs?
# Defaults to `macos`. Other platforms render and report diffs but
# don't fail the test. See docs/reference/gui/screenshot-testing.md.
# TRUCE_SCREENSHOT_REFERENCE_OS = "macos"
