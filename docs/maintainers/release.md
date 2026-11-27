# Releasing Treasury

## Cutting a release

1. Make sure `main` is green in CI.
2. If the release deserves more than its changelog section, write its notes for the people
   downloading it in `docs/release-notes/<version>.md` and commit them.
3. Run `cargo xtask release <version>` (or `scripts/docker-check.sh release <version>` without a
   local toolchain). It sets the version in every manifest, regenerates `CHANGELOG.md`, commits
   `chore(release): <version>` and creates the tag `v<version>`.
4. Push both: `git push origin main v<version>`. The `release` workflow builds, signs, publishes
   and then verifies the release.

## What the release workflow does

1. Checks that the tag matches the workspace version (`cargo xtask check-version`).
2. Runs every CI check on the tagged commit, by calling `ci.yml`.
3. Builds the desktop packages, each on its own platform:

   | Platform | Runner | Packages |
   |---|---|---|
   | Linux x64 | ubuntu-22.04 | `.AppImage`, `.deb`, `.rpm` |
   | Linux arm64 | ubuntu-22.04-arm | `.AppImage`, `.deb`, `.rpm` |
   | macOS (Apple silicon and Intel) | macos-latest | `.dmg`, `.app.tar.gz` for updates |
   | Windows x64 | windows-latest | `.msi`, `-setup.exe` |

   Every package the updater installs is signed with the updater key; the signature is the
   `.sig` file next to it.
4. Publishes one release, marked latest, with every package, `SHA256SUMS`, a CycloneDX SBOM,
   build provenance attestations, and `latest.json`, the feed the app's updater reads. The feed
   is written by `cargo xtask latest-json` from the signed packages. The notes are
   `docs/release-notes/<version>.md` when that file exists, written for the people downloading
   the release; otherwise they are the version's section of the changelog, from the Conventional
   Commits since the previous tag (git-cliff).
5. Downloads the published release, checks every checksum, and checks that every URL in
   `latest.json` answers and that the latest release's feed names the new version.

The app checks `releases/latest/download/latest.json` once at start unless the user turned the
check off in Settings. It installs nothing without the user choosing "Install and restart", and
refuses a package whose signature does not match the public key in `crates/app/tauri.conf.json`.

## Signing material

The maintainer keeps the private keys and their passwords outside the repository, with an
offline backup. Losing the updater key or the Android keystore means existing installs can no
longer receive updates.

The updater key (minisign, password-protected; key id `83BDA098DAA0A99A`) was made by the
maintainer with `tauri signer generate` and is kept by them; its public key is in the app's
`tauri.conf.json`. The Android keystore is in `~/treasury-signing/` (mode 0700) on the machine
that made it:

| File | Purpose |
|---|---|
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

The release workflow uses the two `TAURI_SIGNING_*` secrets and stops with an error when they
are missing. The Android secrets are for the Android app, which is on the roadmap and not built
yet.

Optional, not set: Apple Developer ID and notarization (macOS), Apple distribution certificate
and provisioning profile (iOS), and Windows code signing. Without them, macOS builds are signed
ad hoc (`signingIdentity: "-"`), the iOS app is not built, and Windows installers are unsigned;
the download page explains what users see.

## Repository settings

Applied once, in the repository settings:

- Description, website (`https://sepehrmohseni.github.io/treasury/`) and topics.
- Pages: source is GitHub Actions.
- Dependency graph, Dependabot alerts, malware alerts and security updates on.
- Secret scanning and push protection on; private vulnerability reporting on.
- Actions: the default `GITHUB_TOKEN` stays read-only. Each workflow job requests the permissions
  it needs (`contents: write` only in the job that creates the release).
