# Releasing Treasury

## Cutting a release

1. Make sure `main` is green in CI.
2. Run `cargo xtask release <version>` (or `scripts/docker-check.sh release <version>` without a
   local toolchain). It sets the version in every manifest, regenerates `CHANGELOG.md`, commits
   `chore(release): <version>` and creates the tag `v<version>`.
3. Push both: `git push origin main v<version>`. The `release` workflow builds, signs, publishes
   and then verifies the release.

## Signing material

The maintainer keeps the private keys and their passwords outside the repository, in
`~/treasury-signing/` (mode 0700), with an offline backup. Losing the updater key or the Android
keystore means existing installs can no longer receive updates.

| File | Purpose |
|---|---|
| `updater.key` | Tauri updater private key (minisign, password-protected) |
| `updater.key.password` | Its password |
| `updater.key.pub` | Public key, also in the app's `tauri.conf.json` |
| `android-release.jks` | Android release keystore (PKCS12), alias `treasury`, RSA 4096, valid until 2054 |
| `android-release.jks.base64` | The same keystore, base64-encoded for the secret below |
| `android-keystore.password` | Store and key password |

Android release certificate SHA-256:
`e0c040349e119186ba84fd6f1cec95dd1b592a5d081c4fdb2d475ec46860d74f`. The release workflow refuses
an APK signed with any other certificate.

## Repository secrets

Names only; the values live in the files above.

| Secret | Source file | Used by |
|---|---|---|
| `TAURI_SIGNING_PRIVATE_KEY` | `updater.key` (contents) | signing the desktop update files |
| `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` | `updater.key.password` | the same |
| `ANDROID_KEYSTORE_BASE64` | `android-release.jks.base64` | signing the APK and AAB |
| `ANDROID_KEYSTORE_PASSWORD` | `android-keystore.password` | the same |

Optional, not set: Apple Developer ID and notarization (macOS), Apple distribution certificate
and provisioning profile (iOS), and Windows code signing. Without them, macOS builds are signed
ad hoc, the iOS app is not built, and Windows installers are unsigned; the download page explains
what users see.

## Repository settings

Applied once, in the repository settings:

- Description, website (`https://sepehrmohseni.github.io/treasury/`) and topics.
- Pages: source is GitHub Actions.
- Dependency graph, Dependabot alerts, malware alerts and security updates on.
- Secret scanning and push protection on; private vulnerability reporting on.
- Actions: the default `GITHUB_TOKEN` stays read-only. Each workflow job requests the permissions
  it needs (`contents: write` only in the job that creates the release).
