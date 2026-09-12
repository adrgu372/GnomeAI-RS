# GnomeAI-RS 3.0 — Android & Multi-Device Preview

Over the last previews, GnomeAI-RS has evolved from a primarily desktop
application into a real multi-device client, with a native Android app,
secure phone-to-PC pairing, access to desktop conversations and transport
over Tor/Arti.

The Android app is built with **.NET 10 + Avalonia**, uses the same GnomeAI
client as the desktop and loads the Rust core through `libgnomeai_core.so`.
There is no separate Java/Kotlin app; the Android integration for Activity,
Service, camera, permissions, FileProvider and lifecycle is done through
.NET for Android.

## Android

A complete Android ARM64 client was created, with an installable APK and
automated build through:

```bash
bash scripts/build-android.sh
```

The resulting APK is published as:

```text
ui/GnomeAI.UI.Android/bin/Release/net10.0-android/android-arm64/publish/io.github.adrgu372.gnomeai-Signed.apk
```

The Android client can use the GnomeAI providers, conversations and
streaming, web search, vision/image attachments, files/PDFs and sub-agents.
Native camera integration, a file picker, Android lifecycle handling and a
foreground service for the multi-device features were added.

The native QR pairing scanner was also implemented. The scanner uses the
Android camera directly, NV21 preview, ZXing with `TryHarder`, `AutoRotate`,
QR-only decoding, normal/inverted image and additional crops. There is also
`Scan sharp capture`, which takes a higher-resolution JPEG photo and tries to
decode it separately from the preview. For very dense QR codes there is also
`Enlarge QR`, which proved necessary for reliable pairing.

## Pairing and Mesh

The phone and the desktop can be paired through a QR code and a shared
confirmation code displayed on both devices.

Pairing uses a persistent device identity, ephemeral ECDH P-256 keys, a
one-time invitation secret and SAS confirmation before a peer is accepted.
Invitations are temporary and are consumed after pairing.

The multi-device transport uses **Tor/Arti**, including Onion Services. The
connection was tested not only on the same Wi-Fi network, but also between a
PC and a phone on **5G**, so it does not depend on LAN or port forwarding.

On Android, the identity and pairing material are protected using the
Android Keystore. The desktop keeps the identity and peers in:

```text
${XDG_STATE_HOME:-$HOME/.local/state}/gnomeai-rs/store/devices/devices.json
```

## Desktop client

The desktop client was extended to understand remote devices, not only local
sessions.

The `DeviceHub`, `PeerLink`, `PeerTransport`, `DeviceSessionClient`
infrastructure and support for remote conversations were introduced. The
desktop can see the phone as a peer and the phone can join the PC's
conversations.

Work was also done on a device selector in the interface, so the same app can
display:

```text
This PC
Phone
other paired devices
```

The mechanisms for remote approvals were also added, so that a tool approval
raised on another device can be accepted from the UI without being sent to
the wrong peer by accident.

The approval context is captured when the card is created:

```csharp
var approvalPeer = _selectedPeer;
var approvalSession = _currentSessionId;
```

so changing the selected device before pressing the button does not change
the approval's destination.

The foundations for **Move session**, workspace sync and transferring
conversations/sessions between devices were also introduced.

## Build and compatibility

During the port, several Android/Avalonia-specific problems were solved.

The Activity theme was moved to AppCompat to avoid the crash at startup.
`CameraFileProvider` was fixed for `Android.Content` and
`AndroidX.Core.Content.FileProvider`.

For the Android Release publish, the following was required:

```xml
<TrimmerRootAssembly Include="GnomeAI.Client" RootMode="All" />
```

The recurring conflicts between Avalonia and Android namespaces were also
resolved:

```csharp
using Button = Avalonia.Controls.Button;
using CheckBox = Avalonia.Controls.CheckBox;
using Orientation = Avalonia.Layout.Orientation;
```

and in the scanner:

```csharp
using Environment = System.Environment;
```

Desktop and Android are built separately with:

```bash
bash scripts/build-deb.sh
bash scripts/build-android.sh
```

The goal for the next previews is for both builds to be validated
automatically before publishing, so that compilation fixes no longer need to
be reapplied manually.

## What works now

Preview 7 essentially has a complete flow:

```text
PC starts GnomeAI
        |
generates QR invitation
        |
phone scans the QR
        |
both devices confirm the code
        |
persistent pairing
        |
Tor / Onion Service
        |
phone sees the PC
        |
phone can open the PC's conversations
```

The connection was tested with the phone moved to mobile data as well.

## Known issues

The current version is still a **preview**, not a final release.

There are still a few important issues: the desktop does not yet allow
`Forget device`, and a Forget done from the phone is not propagated to the
PC, so the desktop keeps trying to reconnect; the desktop selector does not
yet allow returning correctly from `Phone` to `This PC`; Android can become
unresponsive while following a remote conversation when the model generates
very many streaming updates; and Android idle battery usage is currently far
too high and must be investigated before release.

There are also a few UI adjustments: `Copy response` and `Select text` must
be removed from the desktop, and on Android they must be moved below the
message content.

In addition, the Android/Desktop build fixes must be consolidated into the
main source so the same errors do not reappear between previews.

---

### Status

**Android APK:** working
**Desktop Debian client:** working
**PC <-> Android pairing:** working
**Tor / 5G remote connection:** working
**Remote conversations:** working
**QR scanner:** working with `Enlarge QR` for dense invitations
**Production ready:** not yet — battery usage, remote streaming and the peer
lifecycle still need work

In short, the important part is that GnomeAI-RS is no longer just "the
desktop app + a separate mobile client". There is already the foundation of a
**GnomeAI device network**, where the PC and the phone can pair,
authenticate and communicate directly over the Mesh/Tor infrastructure.
