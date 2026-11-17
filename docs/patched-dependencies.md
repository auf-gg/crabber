# Patched dependencies

Wired in through `[patch.crates-io]` in the root `Cargo.toml` as git dependencies on our forks in
the auf-gg organisation. Each fork branch is the published release with our changes as commits
on top; `Cargo.lock` pins the commit. After pushing to a fork branch, `cargo update -p <crate>`
picks it up. On a version bump, rebase the branch onto the new release tag (gpui-pre-wgpu has no
git history: import the new crate as published in a commit of its own, keeping the patched file).

| Crate | Fork, branch | Upstream |
|---|---|---|
| gpui-pre-wgpu 0.3.12 | [auf-gg/gpui-pre-wgpu](https://github.com/auf-gg/gpui-pre-wgpu) `main` | snapshot of Zed's `gpui_wgpu`; the same change for Zed is on [auf-gg/zed](https://github.com/auf-gg/zed) `gpui-wgpu-linux-color-emoji` |
| tokio-xmpp 6.0.0 | [auf-gg/xmpp-rs](https://github.com/auf-gg/xmpp-rs) `crabber` | [xmpp-rs on GitLab](https://gitlab.com/xmpp-rs/xmpp-rs) |

Sending changes upstream: xmpp-rs's `CONTRIBUTING.md` refuses any change made with the aid of AI
tools, so our tokio-xmpp commits cannot go there as they are. Zed accepts AI-assisted code only
with a human author who understands it, writes the PR description and replies, and has run Zed
by hand (screenshots for visual changes, CLA signed).

`sasl` is patched to the same xmpp-rs checkout: tokio-xmpp takes it by path, and its
`ChannelBinding` crosses into our connector, so both must be one copy.

## gpui-pre-wgpu 0.3.12

Linux text rendering (`src/cosmic_text_system.rs`): colour emoji from Noto Color Emoji when it is
installed. Without the patch emoji come out black and white whenever another installed font has
them (DejaVu Sans, Noto Sans Symbols, Nerd Fonts), and an emoji written with U+FE0F (❤️, ©️, 1️⃣)
uses the plain glyph of the UI font.

1. `load_family`: a face without the letter `m` is not dropped if it is the known colour emoji font,
   so it can be loaded as a family at all.
2. `with_emoji_fallback`: Noto Color Emoji joins every font's fallback chain, ahead of
   cosmic-text's own fallbacks. gpui otherwise drops the app's fallbacks when the requested family
   is missing (the default `.SystemUIFont` maps to IBM Plex Sans).
3. `emoji_slot`: a grapheme with U+FE0F or U+20E3 goes to the emoji font even where the primary font
   has a glyph for its first character.

Windows is not affected: it renders text through DirectWrite (`gpui-pre-windows`).


## tokio-xmpp 6.0.0

Login the way current servers and clients do it. Upstream only has classic SASL followed by
resource binding.

1. `src/client/sasl2.rs` (new), `src/client/login.rs` `client_login`: XEP-0388 SASL2 where the
   server offers it. After `<success/>` there is no stream restart; the server sends fresh stream
   features, read through the new `XmppStreamElement::Features` (and `Sasl2`) variants in
   `src/xmlstream/xmpp.rs`.
2. XEP-0386 Bind 2: the resource is bound inside the SASL2 exchange. A stream that has a session to
   resume skips it, so the stream negotiation can resume that session as before. For that the
   reconnector gets a `resumable` flag (`src/stanzastream/worker.rs`), `Connection` carries the
   `bound` JID, and `NegotiationState` (`negotiation.rs`) gets a `Bound` state that skips the
   bind IQ. Bind 2 servers drop offline messages: they come from MAM instead, which the app syncs
   after every login.
3. XEP-0484 FAST: `LoginOptions::fast` holds a token (HT-SHA-256-EXPR on TLS 1.3, else -NONE). A
   password login asks for one; a token login proves itself and checks the server's proof. A
   refused token is cleared. `FastToken`'s `Debug` hides the secret.
4. SCRAM fixes for both SASL profiles:
   - The server signature is now checked (`mechanism.success`). Upstream accepted any `<success/>`.
   - When we have a channel binding but the server offers no `-PLUS`, it is plain SCRAM with the
     `y` flag. Upstream fell through to PLAIN.
   - XEP-0474: a `h=` in the server-first-message must hash the offered mechanisms and XEP-0440
     channel-binding types, or the login stops before our proof is sent.
5. API: `Client::new_with_login`, `StanzaStream::new_c2s_with`, `LoginOptions` and `FastToken`
   exported from the crate root; new `AuthError` variants `Downgrade`, `ServerProof`, `Sasl2`.
   `client_auth` is gone (`client_login` replaces it).
6. `src/stanzastream/mod.rs` `split`: `StanzaSender` and `StanzaReceiver` each own their end of
   the channels. Upstream (6.0 and main) kept both halves behind one mutex, and a receiver that
   found it held, by a send waiting for room in the outgoing queue, returned `Pending` without
   a waker. The client worker that reads the stream then slept for good: the socket went unread
   and no timeout was noticed, so there was no reconnect either. One clean login in five got
   stuck that way on the test stand; the race needs only a send and a receive at the same time.
7. `src/client/mod.rs`: `ClientHandle` (from `Client::handle`, exported from the crate root)
   sends from other tasks; `StanzaSender` is `Clone` for it. Upstream sends only through
   `&mut Client`, so a send waiting for room in the outgoing queue, which fills up while the
   stream reconnects, held up the task reading the client as well. The stream closes once no
   handle is left: drop them all before `Client::send_end`.
8. Connection loss reaches the app. `src/client/worker.rs` turns `StreamEvent::Suspended` into
   `Event::Disconnected`; upstream dropped it, so a client never heard of a lost connection.
   `Online { resumed }` follows once it is back. `LoginOptions::on_error` (`src/client/sasl2.rs`,
   called by the reconnector in `src/stanzastream/mod.rs`) is told of each failed login attempt,
   a wrong password included. A refused FAST token is not reported: the password follows right
   away.

Its own tests, including the new ones for the patches, run from the fork's workspace:
`cargo test -p tokio-xmpp --lib --no-default-features --features starttls,ring,rustls-native-certs`.
