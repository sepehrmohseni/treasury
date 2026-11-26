# 0006: Secret stores through keyring-core, with a sealed file as the fallback

## Context

The `keyring` crate's 4.x line is a thin layer over `keyring-core` and per-platform store crates.
Its default configuration links libdbus on Linux, a C dependency the pure-Rust zbus client makes
unnecessary.

## Decision

`treasury-vault` depends on `keyring-core` directly with the macOS keychain store (data
protection keychain on iOS, which needs the app's entitlements), the Windows Credential Manager
store and the zbus Secret Service store with RustCrypto. Where no store is available (a headless
Linux without a Secret Service) the user chooses a passphrase for a vault file sealed with
Argon2id (64 MiB, 3 passes, 4 lanes) and XChaCha20-Poly1305. Android gets its store from the app.

## Consequences

No OpenSSL or libdbus in the dependency tree. The fallback file is only as strong as the
passphrase; the settings screen says which store is in use.
