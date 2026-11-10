# Patched dependencies

Wired in through `[patch.crates-io]` in the root `Cargo.toml`. Each copy is the published crate
with a few lines changed, marked `crabber patch` in the source. On a version bump, copy the new
release from `~/.cargo/registry/src/*/` and re-apply the changes.

## gpui-pre-wgpu 0.3.11

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
