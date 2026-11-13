<div align="center">

<img src="icon.svg" width="120" alt="Crabber logo">

# Crabber

**A fast, GPU-accelerated XMPP messenger written in Rust.**
Encrypted chats, voice and video calls, screen sharing — on Linux and Windows.

[![Build](https://github.com/auf-gg/crabber/actions/workflows/build.yml/badge.svg)](https://github.com/auf-gg/crabber/actions/workflows/build.yml)
[![Release](https://img.shields.io/github/v/release/auf-gg/crabber?color=orange)](https://github.com/auf-gg/crabber/releases/latest)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
![Rust](https://img.shields.io/badge/Rust-2024-b7410e?logo=rust&logoColor=white)
![Platforms](https://img.shields.io/badge/Linux%20%7C%20Windows-blue)
[![AUR](https://img.shields.io/aur/version/crabber-bin?logo=archlinux&logoColor=white)](https://aur.archlinux.org/packages/crabber-bin)

[Features](#features) · [Installation](#installation) · [XEP support](#xep-support) · [Building](#building-from-source)

</div>

Crabber is a desktop client for [XMPP](https://xmpp.org), the open and federated messaging
standard. The interface is drawn on the GPU with [GPUI](https://github.com/zed-industries/gpui)
(the framework behind the Zed editor), so it stays smooth on long histories and big rooms.

## Features

### 💬 Messaging
- Message history in SQLite with per-chat search
- Replies, reactions, edits and retraction (also moderated, in rooms)
- Group chats (MUC) with kick, ban and moderator tools; bookmarks sync between devices
- Sign-up straight from the login screen (in-band registration, CAPTCHAs included)
- Read ticks and unread counters that follow your other devices

### 🔒 Privacy and security
- OMEMO end-to-end encryption (libsignal-compatible) and PGP support
- Encrypted file transfer
- STARTTLS/TLS on rustls, SCRAM-SHA-256-PLUS with channel binding
- Credentials kept in the system keyring

### 📞 Calls
- Voice and video calls over WebRTC (Jingle), ringing on all your devices
- Screen sharing of a chosen monitor or window, up to its full resolution, with system audio
  (other apps only, never the call itself); the computer stays awake during a call. Video needs
  ffmpeg on PATH; capture goes through the desktop portal and PipeWire on Wayland and
  Windows.Graphics.Capture on Windows
- Group audio calls in rooms: everyone in the room sees the call going on and can join
- Microphone, speaker and camera chosen in settings, switched on the fly during a call
- Noise suppression for the microphone ([nnnoiseless](https://github.com/jneem/nnnoiseless), RNNoise in Rust)
- Voice messages (Ogg Opus, compatible with Conversations)

### 🎨 Desktop
- Tray icon and native notifications
- Light/dark themes following the system, color schemes with a custom color editor,
  and Material You colors on Linux from [matugen](https://github.com/InioX/matugen) or
  [dux-shell](https://github.com/duxlabs-dev/dux-shell)
- Linux (X11/Wayland, Vulkan) and Windows (x64/ARM64)

## XEP support

[XMPP extensions](https://xmpp.org/extensions/) Crabber implements, and what is still missing
against the client requirements of the [XMPP Compliance Suites 2023](https://xmpp.org/extensions/xep-0479.html)
(IM and A/V calling) and against current clients. Obsolete and mobile-only extensions are left out.

### Implemented

✅ done · 🟡 partial

| XEP | Name | Status | Notes |
| --- | --- | --- | --- |
| [0027](https://xmpp.org/extensions/xep-0027.html) | Current Jabber OpenPGP Usage | ✅ | Text only, no files; obsolete at the XSF, but still the PGP most clients speak |
| [0030](https://xmpp.org/extensions/xep-0030.html) | Service Discovery | ✅ | |
| [0045](https://xmpp.org/extensions/xep-0045.html) | Multi-User Chat | ✅ | Join, create, invite; kick, ban, moderator role from the group card |
| [0047](https://xmpp.org/extensions/xep-0047.html) | In-Band Bytestreams | ✅ | Over IQs, for Jingle file transfer |
| [0054](https://xmpp.org/extensions/xep-0054.html) | vcard-temp | ✅ | Profile in the contact card, own profile edited in settings; avatars |
| [0066](https://xmpp.org/extensions/xep-0066.html) | Out of Band Data | ✅ | |
| [0077](https://xmpp.org/extensions/xep-0077.html) | In-Band Registration | ✅ | Sign-up from the login screen, picture CAPTCHAs (XEP-0158/0231) included |
| [0084](https://xmpp.org/extensions/xep-0084.html) | User Avatar (PEP) | ✅ | Ours goes to PEP, and to the vCard unless the server copies it there (XEP-0398) |
| [0085](https://xmpp.org/extensions/xep-0085.html) | Chat State Notifications | ✅ | Typing indicator |
| [0115](https://xmpp.org/extensions/xep-0115.html) | Entity Capabilities | ✅ | |
| [0153](https://xmpp.org/extensions/xep-0153.html) | vCard-Based Avatars | ✅ | Our avatar hash in every presence, rooms included |
| [0163](https://xmpp.org/extensions/xep-0163.html) | Personal Eventing Protocol | ✅ | Avatars, bookmarks, read markers, OMEMO |
| [0166](https://xmpp.org/extensions/xep-0166.html) | Jingle | ✅ | Calls; Jingle errors, tie-breaking, content add/modify/remove; file transfer is XEP-0234 |
| [0167](https://xmpp.org/extensions/xep-0167.html) | Jingle RTP Sessions | ✅ | Opus audio (G.711 µ-law fallback), VP8 video, video added mid-call; ringing, mute and hold info |
| [0172](https://xmpp.org/extensions/xep-0172.html) | User Nickname | ✅ | Ours from the profile's nickname (or name); contacts' shown where we have no name for them; sent with subscription requests |
| [0176](https://xmpp.org/extensions/xep-0176.html) | Jingle ICE-UDP Transport | ✅ | |
| [0184](https://xmpp.org/extensions/xep-0184.html) | Message Delivery Receipts | ✅ | |
| [0191](https://xmpp.org/extensions/xep-0191.html) | Blocking Command | ✅ | Contact card, chat banner, subscription requests |
| [0198](https://xmpp.org/extensions/xep-0198.html) | Stream Management | ✅ | Provided by tokio-xmpp |
| [0199](https://xmpp.org/extensions/xep-0199.html) | XMPP Ping | ✅ | Keepalive after 60 s of silence; a dead link is noticed in ~1.5 min |
| [0203](https://xmpp.org/extensions/xep-0203.html) | Delayed Delivery | ✅ | |
| [0215](https://xmpp.org/extensions/xep-0215.html) | External Service Discovery | ✅ | STUN/TURN for calls |
| [0223](https://xmpp.org/extensions/xep-0223.html) | Persistent Storage of Private Data via PubSub | ✅ | Private bookmarks and read markers; open OMEMO bundles |
| [0234](https://xmpp.org/extensions/xep-0234.html) | Jingle File Transfer | ✅ | 1:1, unencrypted: sent when the server has no HTTP Upload; offers arrive as files to download. Slow where the server throttles clients (ejabberd: 3 KB/s by default) |
| [0245](https://xmpp.org/extensions/xep-0245.html) | The /me Command | ✅ | Shown as an action in the chat, the chat list and notifications |
| [0249](https://xmpp.org/extensions/xep-0249.html) | Direct MUC Invitations | ✅ | |
| [0261](https://xmpp.org/extensions/xep-0261.html) | Jingle In-Band Bytestreams Transport Method | ✅ | The only transport: SOCKS5 (XEP-0260) is neither offered nor advertised |
| [0272](https://xmpp.org/extensions/xep-0272.html) | Multiparty Jingle (Muji) | ✅ | Group audio calls in a room, peer-to-peer mesh; call negotiated in the room itself (as Movim 0.34), no XEP-0482 invites |
| [0280](https://xmpp.org/extensions/xep-0280.html) | Message Carbons | ✅ | |
| [0293](https://xmpp.org/extensions/xep-0293.html) | Jingle RTP Feedback Negotiation | ✅ | |
| [0294](https://xmpp.org/extensions/xep-0294.html) | Jingle RTP Header Extensions Negotiation | ✅ | |
| [0308](https://xmpp.org/extensions/xep-0308.html) | Last Message Correction | ✅ | Text messages; edits only from the original sender |
| [0313](https://xmpp.org/extensions/xep-0313.html) | Message Archive Management | ✅ | Personal archive; rooms with an archive catch up from it instead of the MUC join history |
| [0320](https://xmpp.org/extensions/xep-0320.html) | Use of DTLS-SRTP in Jingle Sessions | ✅ | |
| [0333](https://xmpp.org/extensions/xep-0333.html) | Displayed Markers | ✅ | Read ticks |
| [0334](https://xmpp.org/extensions/xep-0334.html) | Message Processing Hints | ✅ | |
| [0338](https://xmpp.org/extensions/xep-0338.html) | Jingle Grouping Framework | ✅ | |
| [0339](https://xmpp.org/extensions/xep-0339.html) | Source-Specific Media Attributes in Jingle | ✅ | |
| [0353](https://xmpp.org/extensions/xep-0353.html) | Jingle Message Initiation | ✅ | Ringing on all devices |
| [0359](https://xmpp.org/extensions/xep-0359.html) | Unique and Stable Stanza IDs | ✅ | |
| [0363](https://xmpp.org/extensions/xep-0363.html) | HTTP File Upload | ✅ | |
| [0368](https://xmpp.org/extensions/xep-0368.html) | SRV records for XMPP over TLS | ✅ | Direct TLS where the domain publishes it (or a manual host on port 5223), STARTTLS otherwise |
| [0377](https://xmpp.org/extensions/xep-0377.html) | Blocking Command Reports | ✅ | Sent with a block |
| [0380](https://xmpp.org/extensions/xep-0380.html) | Explicit Message Encryption | ✅ | |
| [0384](https://xmpp.org/extensions/xep-0384.html) | OMEMO Encryption | ✅ | Legacy v0.3 namespace |
| [0386](https://xmpp.org/extensions/xep-0386.html) | Bind 2 | ✅ | Resource bound at login; a session to resume is resumed instead |
| [0388](https://xmpp.org/extensions/xep-0388.html) | Extensible SASL Profile (SASL2) | ✅ | SCRAM-SHA-256-PLUS with `tls-exporter` where offered; the server's signature is checked |
| [0392](https://xmpp.org/extensions/xep-0392.html) | Consistent Color Generation | ✅ | Nick colours by the XEP's HSLuv algorithm, lighter on a dark theme; no switch to turn them off |
| [0393](https://xmpp.org/extensions/xep-0393.html) | Message Styling | ✅ | `*bold*`, `_italic_`, `~strike~`, `` `code` ``, quotes and code blocks; the markers are hidden; `<unstyled/>` is not honoured |
| [0398](https://xmpp.org/extensions/xep-0398.html) | User Avatar to vCard-Based Avatars Conversion | ✅ | The server's copy is used instead of publishing the vCard avatar ourselves |
| [0402](https://xmpp.org/extensions/xep-0402.html) | PEP Native Bookmarks | ✅ | Joined rooms sync between devices |
| [0410](https://xmpp.org/extensions/xep-0410.html) | MUC Self-Ping | ✅ | Every 10 min; rejoins a room we dropped out of |
| [0421](https://xmpp.org/extensions/xep-0421.html) | Occupant identifiers for semi-anonymous MUCs | ✅ | Only from rooms that advertise them; corrections and retractions must come from the same occupant |
| [0424](https://xmpp.org/extensions/xep-0424.html) | Message Retraction | ✅ | Delete for everyone, with confirmation |
| [0425](https://xmpp.org/extensions/xep-0425.html) | Moderated Message Retraction | ✅ | `:1`, falls back to `:0` for older servers |
| [0428](https://xmpp.org/extensions/xep-0428.html) | Fallback Indication | 🟡 | Reply quotes only |
| [0440](https://xmpp.org/extensions/xep-0440.html) | SASL Channel-Binding Type Capability | ✅ | `tls-exporter` binding where the server lists it; the list also feeds XEP-0474 |
| [0444](https://xmpp.org/extensions/xep-0444.html) | Message Reactions | ✅ | Quick set in the message menu; groups use the room's stanza-id |
| [0454](https://xmpp.org/extensions/xep-0454.html) | OMEMO Media Sharing | ✅ | `aesgcm://` links |
| [0461](https://xmpp.org/extensions/xep-0461.html) | Message Replies | ✅ | Quote with jump to the original; fallback quote for other clients |
| [0474](https://xmpp.org/extensions/xep-0474.html) | SASL SCRAM Downgrade Protection | ✅ | The server's `h=` is checked against what it offered |
| [0484](https://xmpp.org/extensions/xep-0484.html) | Fast Authentication Streamlining Tokens | ✅ | HT-SHA-256-EXPR (TLS 1.3) or -NONE tokens, kept with the password |
| [0486](https://xmpp.org/extensions/xep-0486.html) | MUC Avatars | ✅ | By the hash in the room's disco info; refreshed when the room's configuration changes |
| [0490](https://xmpp.org/extensions/xep-0490.html) | Message Displayed Synchronization | ✅ | Unread counters follow other devices |

### To do

Crabber covers the **Core** and **Advanced** client levels of the Compliance Suites 2023, the
extensions they recommend, and the newer ones most current clients use. Not done on purpose: the
legacy bookmark storage (XEP-0048/0049) the suites still accept as a fallback.

## Installation

### Arch Linux (AUR)

Release binaries:

```sh
paru -S crabber-bin
```

Or build from the latest git commit:

```sh
paru -S crabber-git
```

### Prebuilt binaries

Grab them from [GitHub Releases](https://github.com/auf-gg/crabber/releases):

- **Windows** — `setup.exe` installer or a portable `.zip` (x86_64 and ARM64)
- **Linux** — `tar.gz` archive (x86_64 and aarch64) with an `install.sh` script
  that installs the app to `~/.local` for the current user

On Linux, emoji are drawn with Noto Color Emoji when it is installed (`noto-fonts-emoji` on Arch,
`fonts-noto-color-emoji` on Debian/Ubuntu); the AUR packages depend on it.

## Building from source

Requires Rust as pinned in [`rust-toolchain.toml`](rust-toolchain.toml) (1.97+).

Linux system dependencies (Debian/Ubuntu package names):

```sh
sudo apt install pkg-config clang libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
  libx11-xcb-dev libxcb1-dev libfontconfig-dev libfreetype-dev \
  libasound2-dev libvulkan-dev libdbus-1-dev
```

Colour emojis need the Noto Color Emoji font (`fonts-noto-color-emoji` on Debian/Ubuntu).
gpui's Linux text rendering is patched for it, see [`vendor/README.md`](vendor/README.md).

On Windows install the Visual Studio Build Tools (MSVC), then on either platform:

```sh
cargo build --release
```

The binary ends up in `target/release/crabber`.

## Packaging

The [`dist/`](dist) directory holds packaging templates:

- [`PKGBUILD-git`](dist/PKGBUILD-git) — AUR `crabber-git`, compiled from source
- [`PKGBUILD-bin`](dist/PKGBUILD-bin) — AUR `crabber-bin`, installed from release binaries
- [`windows/installer.nsi`](dist/windows/installer.nsi) — NSIS installer script

CI workflows keep everything in sync: [`build.yml`](.github/workflows/build.yml)
builds and publishes artifacts for every platform, [`aur.yml`](.github/workflows/aur.yml)
pushes updated PKGBUILDs to the AUR (the `-git` package on every push, the `-bin`
package on every release).

## License

[MIT](LICENSE)
