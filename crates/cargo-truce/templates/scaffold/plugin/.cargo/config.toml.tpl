# Local build-environment config. Gitignored — set your own values.
# `cargo truce install` and `cargo truce package` both read from here.
#
# Cargo injects everything in `[env]` into the environment of any
# subcommand it spawns, so values here are visible to `cargo truce`
# without further plumbing. See `docs/reference/cargo-config.md` for
# the full list of env vars truce understands.

[env]
# --- macOS code signing ---
# TRUCE_SIGNING_IDENTITY           = "Developer ID Application: Your Name (TEAMID)"
# TRUCE_INSTALLER_SIGNING_IDENTITY = "Developer ID Installer: Your Name (TEAMID)"

# --- macOS notarization (set [macos.packaging].notarize = true in truce.toml) ---
# Either set up a keychain profile (preferred):
#     xcrun notarytool store-credentials TRUCE_NOTARY
# …or set explicit credentials here:
# APPLE_ID              = "you@example.com"
# TEAM_ID               = "ABCDEFG123"
# APP_SPECIFIC_PASSWORD = "xxxx-xxxx-xxxx-xxxx"

# --- AAX SDK (macOS and Windows) ---
# AAX_SDK_PATH = "/path/to/aax-sdk-2-9-0"
# AAX_SDK_PATH = 'C:\Users\you\aax-sdk-2-9-0'

# --- Windows Authenticode signing ---
# Pick ONE of: Azure Trusted Signing, cert thumbprint, or .pfx file.
#
# Azure Trusted Signing:
# TRUCE_AZURE_ACCOUNT = "your-account"
# TRUCE_AZURE_PROFILE = "your-cert-profile"
# TRUCE_AZURE_DLIB    = 'C:\Program Files\Microsoft Trusted Signing Client\bin\x64\Azure.CodeSigning.Dlib.dll'
#
# Cert thumbprint (cert already in current user's store):
# TRUCE_CERT_SHA1  = "0123456789abcdef..."
# TRUCE_CERT_STORE = "My"
#
# .pfx file:
# TRUCE_PFX_PATH     = 'C:\path\to\cert.pfx'
# TRUCE_PFX_PASSWORD = "..."

# Optional override for the RFC 3161 timestamp server:
# TRUCE_TIMESTAMP_URL = "http://timestamp.digicert.com"
