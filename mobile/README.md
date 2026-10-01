# Threadlane mobile client (iOS)

A thin GPUI client (gpui-mobile + gpui-kit) that connects to the Threadlane
desktop app and monitors sessions live over the daemon's WebSocket
contract. Read-mostly: watch transcripts stream, see tool activity, and
answer permission prompts; it is not a control surface.

## Pairing

1. Desktop: sidebar footer → **Share with mobile** (phone icon) →
   **Start sharing**. The dialog shows a QR code encoding
   `threadlane://pair?host=…&port=…&token=…` plus the same link as text.
2. iPhone: scan the QR with the Camera app — the registered
   `threadlane://` URL scheme launches the app and auto-fills host, port,
   and token.
3. The desktop's listener is LAN-bound (`0.0.0.0`) and requires the
   pairing token, minted fresh every time sharing starts. Sharing the QR
   = sharing a live capability; hit **Stop sharing** when done.

## Layout

- `rust/` — `threadlane-mobile` crate: `staticlib` + `rlib`, its own
  Cargo workspace (the desktop workspace pins `gpui-pre` 0.3.3 while
  gpui-mobile requires `=0.3.4`). `client.rs` is a minimal WS driver
  (reconnect + `?since=` replay, bearer auth); `app.rs` holds the
  connect/session-list/transcript views.
- `ios/` — XcodeGen container: `App.swift` embeds the GPUI surface and
  forwards deep links; `project.yml` builds `libthreadlane_mobile.a` in
  a pre-build script.

## Build & run (simulator)

```bash
rustup target add aarch64-apple-ios-sim
brew install xcodegen
cd ios && ./build.sh            # or: ./build.sh "iPhone 17"
```

## Build & run (physical iPhone over USB)

1. Add an Apple ID in Xcode → Settings → Accounts — a free "Personal
   Team" is enough for dev installs.
2. Plug the iPhone in over USB and trust the Mac on the phone; enable
   Developer Mode on the phone (Settings → Privacy & Security →
   Developer Mode) if it is not already on.
3. Run:

```bash
cd ios
DEVELOPMENT_TEAM=5VW98ZM2UM ./build.sh device            # first connected iPhone
DEVELOPMENT_TEAM=5VW98ZM2UM ./build.sh device "Sab's iPhone"
```

`DEVELOPMENT_TEAM` is your ten-character team id (Apple Developer
account → Membership, or the Personal Team Xcode created). The script
cross-compiles the Rust staticlib for `aarch64-apple-ios`, builds the
Release app, signs it, and installs + launches via `devicectl`.
Device builds allow Xcode to contact Apple to create or update signing
assets and register the destination device with the selected team. The
Apple account in Xcode must have permission to manage those assets.
If provisioning fails, verify the team selection and account permissions
in Xcode; an installed distribution certificate alone is not enough for
a development install.

On-device pairing works like the simulator path: scan the desktop's QR
with the Camera app while both devices share the same LAN.

Same-machine testing: when the desktop app and the simulator run on the
same Mac, the pairing QR encodes the LAN address, which the simulator
reaches directly — no special config needed.
