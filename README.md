<p align="center">
  <img src="icon.svg" width="128" alt="Crabber logo">
</p>

# Crabber

[![Build](https://github.com/auf-gg/crabber/actions/workflows/build.yml/badge.svg)](https://github.com/auf-gg/crabber/actions/workflows/build.yml)
[![License: Apache-2.0](https://img.shields.io/badge/License-Apache--2.0-blue.svg)](LICENSE)

Cross-platform XMPP messenger with a GPU-accelerated [GPUI](https://github.com/zed-industries/gpui) interface.

## Features

- XMPP instant messaging with STARTTLS/TLS (rustls)
- OMEMO end-to-end encryption (libsignal-compatible) and PGP support
- Encrypted file transfer
- Voice calls over WebRTC
- Message history stored in SQLite with per-chat search
- Account credentials kept in the system keyring
- Desktop integration: tray icon, native notifications
- Linux (X11/Wayland, Vulkan) and Windows (x64/ARM64)

## XEP support

Which [XMPP extensions](https://xmpp.org/extensions/) a modern client needs, and where
Crabber stands on each.

✅ done · 🟡 partial · ❌ to do

| XEP | Name | Status | Notes |
| --- | --- | --- | --- |
| [0027](https://xmpp.org/extensions/xep-0027.html) | Current Jabber OpenPGP Usage | ✅ | Text only, no files |
| [0030](https://xmpp.org/extensions/xep-0030.html) | Service Discovery | ✅ | |
| [0045](https://xmpp.org/extensions/xep-0045.html) | Multi-User Chat | ✅ | Join, create, invite; kick, ban, moderator role from the group card |
| [0047](https://xmpp.org/extensions/xep-0047.html) | In-Band Bytestreams | ❌ | Jingle file transfer fallback |
| [0048](https://xmpp.org/extensions/xep-0048.html) | Bookmarks | ❌ | Legacy; rooms sync via XEP-0402 |
| [0049](https://xmpp.org/extensions/xep-0049.html) | Private XML Storage | ❌ | |
| [0054](https://xmpp.org/extensions/xep-0054.html) | vcard-temp | 🟡 | Avatars only |
| [0065](https://xmpp.org/extensions/xep-0065.html) | SOCKS5 Bytestreams | ❌ | Jingle file transfer |
| [0084](https://xmpp.org/extensions/xep-0084.html) | User Avatar (PEP) | ✅ | Ours is published to both PEP and vCard |
| [0085](https://xmpp.org/extensions/xep-0085.html) | Chat State Notifications | ✅ | Typing indicator |
| [0086](https://xmpp.org/extensions/xep-0086.html) | Error Condition Mappings | ❌ | Legacy error codes |
| [0092](https://xmpp.org/extensions/xep-0092.html) | Software Version | ❌ | |
| [0115](https://xmpp.org/extensions/xep-0115.html) | Entity Capabilities | ✅ | |
| [0153](https://xmpp.org/extensions/xep-0153.html) | vCard-Based Avatars | ✅ | |
| [0163](https://xmpp.org/extensions/xep-0163.html) | Personal Eventing Protocol | 🟡 | OMEMO device lists and bundles only |
| [0166](https://xmpp.org/extensions/xep-0166.html) | Jingle | 🟡 | Audio calls; no file transfer |
| [0167](https://xmpp.org/extensions/xep-0167.html) | Jingle RTP Sessions | 🟡 | Audio only (G.711 µ-law), no video |
| [0172](https://xmpp.org/extensions/xep-0172.html) | User Nickname | ❌ | |
| [0176](https://xmpp.org/extensions/xep-0176.html) | Jingle ICE-UDP Transport | ✅ | |
| [0184](https://xmpp.org/extensions/xep-0184.html) | Message Delivery Receipts | ✅ | |
| [0191](https://xmpp.org/extensions/xep-0191.html) | Blocking Command | ✅ | Contact card, chat banner, subscription requests |
| [0198](https://xmpp.org/extensions/xep-0198.html) | Stream Management | ✅ | Provided by tokio-xmpp |
| [0199](https://xmpp.org/extensions/xep-0199.html) | XMPP Ping | ✅ | Keepalive after 60 s of silence; a dead link is noticed in ~1.5 min |
| [0202](https://xmpp.org/extensions/xep-0202.html) | Entity Time | ❌ | |
| [0215](https://xmpp.org/extensions/xep-0215.html) | External Service Discovery | ✅ | STUN/TURN for calls |
| [0223](https://xmpp.org/extensions/xep-0223.html) | Persistent Storage of Private Data via PubSub | 🟡 | OMEMO publish options only |
| [0234](https://xmpp.org/extensions/xep-0234.html) | Jingle File Transfer | ❌ | Files go over HTTP Upload |
| [0237](https://xmpp.org/extensions/xep-0237.html) | Roster Versioning | ❌ | Full roster fetched on every login |
| [0245](https://xmpp.org/extensions/xep-0245.html) | The /me Command | ❌ | |
| [0249](https://xmpp.org/extensions/xep-0249.html) | Direct MUC Invitations | ✅ | |
| [0260](https://xmpp.org/extensions/xep-0260.html) | Jingle SOCKS5 Bytestreams Transport | ❌ | |
| [0261](https://xmpp.org/extensions/xep-0261.html) | Jingle In-Band Bytestreams Transport | ❌ | |
| [0280](https://xmpp.org/extensions/xep-0280.html) | Message Carbons | ✅ | |
| [0293](https://xmpp.org/extensions/xep-0293.html) | Jingle RTP Feedback Negotiation | ✅ | |
| [0294](https://xmpp.org/extensions/xep-0294.html) | Jingle RTP Header Extensions Negotiation | ✅ | |
| [0308](https://xmpp.org/extensions/xep-0308.html) | Last Message Correction | ✅ | Text messages; edits only from the original sender |
| [0313](https://xmpp.org/extensions/xep-0313.html) | Message Archive Management | ✅ | |
| [0317](https://xmpp.org/extensions/xep-0317.html) | Hats | ❌ | |
| [0319](https://xmpp.org/extensions/xep-0319.html) | Last User Interaction in Presence | ❌ | |
| [0320](https://xmpp.org/extensions/xep-0320.html) | Use of DTLS-SRTP in Jingle Sessions | ✅ | |
| [0333](https://xmpp.org/extensions/xep-0333.html) | Chat Markers | ✅ | Read ticks |
| [0338](https://xmpp.org/extensions/xep-0338.html) | Jingle Grouping Framework | ✅ | |
| [0339](https://xmpp.org/extensions/xep-0339.html) | Source-Specific Media Attributes in Jingle | ✅ | |
| [0352](https://xmpp.org/extensions/xep-0352.html) | Client State Indication | ❌ | Needs nonza sending in tokio-xmpp |
| [0353](https://xmpp.org/extensions/xep-0353.html) | Jingle Message Initiation | ✅ | Ringing on all devices |
| [0357](https://xmpp.org/extensions/xep-0357.html) | Push Notifications | ❌ | Mobile-oriented, low priority on desktop |
| [0363](https://xmpp.org/extensions/xep-0363.html) | HTTP File Upload | ✅ | |
| [0368](https://xmpp.org/extensions/xep-0368.html) | SRV records for XMPP over TLS | ❌ | STARTTLS only |
| [0377](https://xmpp.org/extensions/xep-0377.html) | Spam Reporting | ✅ | Sent with a block |
| [0384](https://xmpp.org/extensions/xep-0384.html) | OMEMO Encryption | ✅ | Legacy v0.3 namespace |
| [0386](https://xmpp.org/extensions/xep-0386.html) | Bind 2 | ❌ | |
| [0388](https://xmpp.org/extensions/xep-0388.html) | Extensible SASL Profile (SASL2) | ❌ | SCRAM over classic SASL |
| [0390](https://xmpp.org/extensions/xep-0390.html) | Entity Capabilities 2.0 | ❌ | |
| [0391](https://xmpp.org/extensions/xep-0391.html) | Jingle Encrypted Transports | ❌ | |
| [0392](https://xmpp.org/extensions/xep-0392.html) | Consistent Color Generation | 🟡 | Stable per-nick colours, not the XEP algorithm |
| [0393](https://xmpp.org/extensions/xep-0393.html) | Message Styling | 🟡 | Code blocks and inline code only |
| [0396](https://xmpp.org/extensions/xep-0396.html) | Jingle Encrypted Transports - OMEMO | ❌ | |
| [0398](https://xmpp.org/extensions/xep-0398.html) | User Avatar to vCard-Based Avatars Conversion | ❌ | |
| [0402](https://xmpp.org/extensions/xep-0402.html) | PEP Native Bookmarks | ✅ | Joined rooms sync between devices |
| [0410](https://xmpp.org/extensions/xep-0410.html) | MUC Self-Ping | ✅ | Every 10 min; rejoins a room we dropped out of |
| [0411](https://xmpp.org/extensions/xep-0411.html) | Bookmarks Conversion | ❌ | |
| [0421](https://xmpp.org/extensions/xep-0421.html) | Occupant identifiers for semi-anonymous MUCs | ❌ | |
| [0424](https://xmpp.org/extensions/xep-0424.html) | Message Retraction | ✅ | Delete for everyone, with confirmation |
| [0425](https://xmpp.org/extensions/xep-0425.html) | Moderated Message Retraction | ✅ | `:1`, falls back to `:0` for older servers |
| [0428](https://xmpp.org/extensions/xep-0428.html) | Fallback Indication | 🟡 | Reply quotes only |
| [0440](https://xmpp.org/extensions/xep-0440.html) | SASL Channel-Binding Type Capability | 🟡 | `tls-exporter` binding, capability not read |
| [0444](https://xmpp.org/extensions/xep-0444.html) | Message Reactions | ✅ | Quick set in the message menu; groups use the room's stanza-id |
| [0454](https://xmpp.org/extensions/xep-0454.html) | OMEMO Media Sharing | ✅ | `aesgcm://` links |
| [0455](https://xmpp.org/extensions/xep-0455.html) | Service Outage Status | ❌ | |
| [0461](https://xmpp.org/extensions/xep-0461.html) | Message Replies | ✅ | Quote with jump to the original; fallback quote for other clients |
| [0474](https://xmpp.org/extensions/xep-0474.html) | SASL SCRAM Downgrade Protection | ❌ | |
| [0484](https://xmpp.org/extensions/xep-0484.html) | Fast Authentication Streamlining Tokens | ❌ | |
| [0486](https://xmpp.org/extensions/xep-0486.html) | MUC Avatars | ❌ | |
| [0490](https://xmpp.org/extensions/xep-0490.html) | Message Displayed Synchronization | ✅ | Unread counters follow other devices |

Also implemented: XEP-0066 (Out of Band Data),
XEP-0203 (Delayed Delivery), XEP-0334 (Message Processing Hints),
XEP-0359 (Unique and Stable Stanza IDs), XEP-0380 (Explicit Message Encryption).

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

## Building from source

Requires Rust as pinned in [`rust-toolchain.toml`](rust-toolchain.toml) (1.97+).

Linux system dependencies (Debian/Ubuntu package names):

```sh
sudo apt install pkg-config clang libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev \
  libx11-xcb-dev libxcb1-dev libfontconfig-dev libfreetype-dev \
  libasound2-dev libvulkan-dev libdbus-1-dev
```

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

[Apache-2.0](LICENSE)
