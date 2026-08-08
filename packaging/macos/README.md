# macOS installer and signing

`scripts/build-macos-arm64.sh` creates a real flat installer package and a DMG
that contains it. The package installs the following menu applications:

- `/Applications/GnomeAI-RS.app`
- `/Applications/GnomeAI-RS Agent.app`
- `/Applications/GnomeAI-RS Web.app`

It also installs `gnomef-rs`, `gnomef-agent`, and `gnomef-web` under
`/usr/local/bin`. Runtime state belongs to the current user and is stored under
`~/Library/Application Support/GnomeAI-RS`.

## Local build

Run on an Apple Silicon Mac:

```bash
bash scripts/build-macos-arm64.sh
```

Without Developer ID credentials, the script applies ad-hoc signatures so the
bundle structure can be tested locally. Ad-hoc signing does not make an
Internet download trusted by Gatekeeper.

## Developer ID and notarization

Public downloads must be signed with both a `Developer ID Application`
identity and a `Developer ID Installer` identity, submitted to Apple's notary
service, and stapled. The build script supports either a local notary keychain
profile or an App Store Connect API key:

```bash
GNOMEAI_APP_SIGN_IDENTITY='Developer ID Application: Example (TEAMID)' \
GNOMEAI_INSTALLER_SIGN_IDENTITY='Developer ID Installer: Example (TEAMID)' \
GNOMEAI_NOTARY_PROFILE='gnomeai-notary' \
bash scripts/build-macos-arm64.sh
```

The GitHub Actions workflow uses these repository secrets:

- `MACOS_SIGNING_P12`: base64-encoded PKCS#12 containing both Developer ID
  identities;
- `MACOS_SIGNING_P12_PASSWORD`: password for that PKCS#12 file;
- `APP_STORE_CONNECT_KEY_P8`: base64-encoded App Store Connect private API key;
- `APP_STORE_CONNECT_KEY_ID`: API key ID;
- `APP_STORE_CONNECT_ISSUER_ID`: API issuer ID.

The workflow builds unsigned/ad-hoc artifacts for pull requests. Pushes and
version tags are signed and notarized when all secrets are configured. A tag
must exactly match the Cargo package version, for example `v1.2.3`.

Apple's requirements and command-line notarization workflow are documented in
[Notarizing macOS software before distribution](https://developer.apple.com/documentation/security/notarizing-macos-software-before-distribution)
and
[Customizing the notarization workflow](https://developer.apple.com/documentation/security/customizing-the-notarization-workflow).
