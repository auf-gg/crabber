//! Design tokens: one palette per mode, also pushed into gpui-component so its
//! buttons, inputs and switches match the app.
#[cfg(target_os = "linux")]
use std::path::PathBuf;

use gpui_kit::component::{ActiveTheme as _, Theme};
use gpui_kit::*;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy)]
pub struct Palette {
    pub dark: bool,
    /// Sidebar, headers, composer, panels.
    pub panel: Hsla,
    /// Chat wallpaper gradient.
    pub chat_top: Hsla,
    pub chat_bottom: Hsla,
    pub accent: Hsla,
    pub accent_hover: Hsla,
    /// Tinted backgrounds (selected toggles, info chips).
    pub accent_soft: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    pub border: Hsla,
    /// Lines between chat list rows.
    pub divider: Hsla,
    pub hover: Hsla,
    /// Filled inputs (search, composer).
    pub field: Hsla,
    pub bubble_in: Hsla,
    pub bubble_in_text: Hsla,
    pub bubble_out: Hsla,
    pub bubble_out_text: Hsla,
    /// Time/ticks inside outgoing bubbles.
    pub meta_out: Hsla,
    pub online: Hsla,
    pub danger: Hsla,
    /// Translucent chips over the wallpaper (date separators).
    pub chip: Hsla,
    pub chip_text: Hsla,
}

fn c(hex: u32) -> Hsla {
    rgb(hex).into()
}

fn ca(hex: u32, alpha: f32) -> Hsla {
    let mut h: Hsla = rgb(hex).into();
    h.a = alpha;
    h
}

pub fn light() -> Palette {
    Palette {
        dark: false,
        panel: c(0xFFFFFF),
        chat_top: c(0xDCE7F3),
        chat_bottom: c(0xEDE7F6),
        accent: c(0x3390EC),
        accent_hover: c(0x2A7FD6),
        accent_soft: c(0xE8F2FD),
        text: c(0x0F1419),
        muted: c(0x707579),
        border: c(0xE7EAEE),
        divider: c(0xEDEFF2),
        hover: c(0xF2F4F6),
        field: c(0xF1F3F5),
        bubble_in: c(0xFFFFFF),
        bubble_in_text: c(0x0F1419),
        bubble_out: c(0x3390EC),
        bubble_out_text: c(0xFFFFFF),
        meta_out: ca(0xFFFFFF, 0.75),
        online: c(0x2EB67D),
        danger: c(0xE53935),
        chip: ca(0x000000, 0.22),
        chip_text: c(0xFFFFFF),
    }
}

pub fn dark() -> Palette {
    Palette {
        dark: true,
        panel: c(0x17212B),
        chat_top: c(0x0E1621),
        chat_bottom: c(0x111D2B),
        accent: c(0x4F9CF0),
        accent_hover: c(0x6AABF3),
        accent_soft: c(0x1E3247),
        text: c(0xF5F7FA),
        muted: c(0x7C8B9C),
        border: c(0x0F1923),
        divider: c(0x223040),
        hover: c(0x202B36),
        field: c(0x242F3D),
        bubble_in: c(0x182533),
        bubble_in_text: c(0xF5F7FA),
        bubble_out: c(0x2B5278),
        bubble_out_text: c(0xFFFFFF),
        meta_out: ca(0xFFFFFF, 0.65),
        online: c(0x4CC38A),
        danger: c(0xEF5350),
        chip: ca(0x000000, 0.35),
        chip_text: c(0xE8EDF2),
    }
}

/// The six colors a color scheme is described with; the rest of the palette is derived from them.
#[derive(Clone, Copy, Serialize, Deserialize)]
pub struct SchemeBase {
    pub accent: u32,
    pub panel: u32,
    pub chat: u32,
    pub bubble_in: u32,
    pub bubble_out: u32,
    pub text: u32,
}

/// A full color scheme: the base colors of both modes.
#[derive(Clone, Copy, Serialize, Deserialize)]
pub struct SchemeColors {
    pub light: SchemeBase,
    pub dark: SchemeBase,
}

/// The built-in palette expressed as a base, so the "custom" editor starts from it.
pub const DEFAULT_BASE: SchemeColors = SchemeColors {
    light: SchemeBase {
        accent: 0x3390EC,
        panel: 0xFFFFFF,
        chat: 0xDCE7F3,
        bubble_in: 0xFFFFFF,
        bubble_out: 0x3390EC,
        text: 0x0F1419,
    },
    dark: SchemeBase {
        accent: 0x4F9CF0,
        panel: 0x17212B,
        chat: 0x0E1621,
        bubble_in: 0x182533,
        bubble_out: 0x2B5278,
        text: 0xF5F7FA,
    },
};

/// Built-in color scheme: id, label key (locales/*.json), base colors.
pub struct Preset(pub &'static str, pub &'static str, pub SchemeColors);

pub const PRESETS: &[Preset] = &[
    Preset(
        "emerald",
        "scheme.emerald",
        SchemeColors {
            light: SchemeBase { accent: 0x1E9E61, panel: 0xFFFFFF, chat: 0xDCF2E7, bubble_in: 0xFFFFFF, bubble_out: 0x2EB67D, text: 0x0E1A14 },
            dark: SchemeBase { accent: 0x34C98B, panel: 0x152019, chat: 0x0E1712, bubble_in: 0x1A2921, bubble_out: 0x1E6B4C, text: 0xF1F7F4 },
        },
    ),
    Preset(
        "sunset",
        "scheme.sunset",
        SchemeColors {
            light: SchemeBase { accent: 0xD95F2B, panel: 0xFFFFFF, chat: 0xFAE6DA, bubble_in: 0xFFFFFF, bubble_out: 0xC95C1E, text: 0x201510 },
            dark: SchemeBase { accent: 0xF0824E, panel: 0x231A15, chat: 0x191009, bubble_in: 0x2B201A, bubble_out: 0x8A4B2F, text: 0xF8F3EF },
        },
    ),
    Preset(
        "lilac",
        "scheme.lilac",
        SchemeColors {
            light: SchemeBase { accent: 0x6B5CE7, panel: 0xFFFFFF, chat: 0xEAE6FB, bubble_in: 0xFFFFFF, bubble_out: 0x5B4DC7, text: 0x161220 },
            dark: SchemeBase { accent: 0x9C8CF2, panel: 0x1C1926, chat: 0x14111F, bubble_in: 0x241F35, bubble_out: 0x4A3F8F, text: 0xF4F2FA },
        },
    ),
    Preset(
        "graphite",
        "scheme.graphite",
        SchemeColors {
            light: SchemeBase { accent: 0x5A646E, panel: 0xFFFFFF, chat: 0xE8ECEF, bubble_in: 0xFFFFFF, bubble_out: 0x6B7680, text: 0x14171A },
            dark: SchemeBase { accent: 0x9AA5B1, panel: 0x15181C, chat: 0x101316, bubble_in: 0x1B1F24, bubble_out: 0x3A424C, text: 0xF0F2F4 },
        },
    ),
];

/// Mixes `a` toward `b` by `t` in RGB space.
fn tint(a: Hsla, b: Hsla, t: f32) -> Hsla {
    let (a, b) = (Rgba::from(a), Rgba::from(b));
    Rgba { r: a.r + (b.r - a.r) * t, g: a.g + (b.g - a.g) * t, b: a.b + (b.b - a.b) * t, a: 1. }.into()
}

/// White or near-black, whichever reads on top of `bg`.
pub fn on_color(bg: Hsla) -> Hsla {
    let r = Rgba::from(bg);
    if r.r * 255. * 0.299 + r.g * 255. * 0.587 + r.b * 255. * 0.114 < 150. { c(0xFFFFFF) } else { c(0x141414) }
}

/// Builds a full palette from a scheme's six base colors.
pub fn from_base(b: &SchemeBase, dark: bool) -> Palette {
    let (panel, chat, accent, text) = (c(b.panel), c(b.chat), c(b.accent), c(b.text));
    let out_text = on_color(c(b.bubble_out));
    Palette {
        dark,
        panel,
        chat_top: chat,
        chat_bottom: tint(chat, panel, 0.45),
        accent,
        accent_hover: if dark { tint(accent, c(0xFFFFFF), 0.12) } else { tint(accent, c(0x000000), 0.1) },
        accent_soft: tint(accent, panel, 0.8),
        text,
        muted: tint(text, panel, if dark { 0.55 } else { 0.6 }),
        border: if dark { tint(panel, c(0x000000), 0.3) } else { tint(panel, text, 0.09) },
        divider: tint(panel, text, if dark { 0.07 } else { 0.06 }),
        hover: tint(panel, text, if dark { 0.05 } else { 0.045 }),
        field: tint(panel, text, if dark { 0.09 } else { 0.065 }),
        bubble_in: c(b.bubble_in),
        bubble_in_text: text,
        bubble_out: c(b.bubble_out),
        bubble_out_text: out_text,
        meta_out: out_text.opacity(0.72),
        online: if dark { c(0x4CC38A) } else { c(0x2EB67D) },
        danger: if dark { c(0xEF5350) } else { c(0xE53935) },
        chip: ca(0x000000, if dark { 0.35 } else { 0.22 }),
        chip_text: if dark { c(0xE8EDF2) } else { c(0xFFFFFF) },
    }
}

/// The scheme override installed by `apply`; `None` fields fall back to the built-in palettes.
#[derive(Default)]
pub struct SchemeState {
    pub light: Option<Palette>,
    pub dark: Option<Palette>,
}

impl Global for SchemeState {}

pub fn palette(cx: &App) -> Palette {
    let is_dark = cx.theme().is_dark();
    cx.try_global::<SchemeState>()
        .and_then(|s| if is_dark { s.dark } else { s.light })
        .unwrap_or_else(|| if is_dark { dark() } else { light() })
}

/// Resolves the scheme selection to per-mode palettes; `(None, None)` = built-in
/// (also the fallback when a system source's file cannot be read).
fn resolve(scheme: &str, custom: Option<SchemeColors>) -> (Option<Palette>, Option<Palette>) {
    match scheme {
        "custom" => {
            let cs = custom.unwrap_or(DEFAULT_BASE);
            (Some(from_base(&cs.light, false)), Some(from_base(&cs.dark, true)))
        }
        #[cfg(target_os = "linux")]
        "matugen" | "dux" => match cache_colors(scheme) {
            Some(cs) => (Some(from_base(&cs.light, false)), Some(from_base(&cs.dark, true))),
            None => (None, None),
        },
        _ => PRESETS
            .iter()
            .find(|p| p.0 == scheme)
            .map(|Preset(_, _, cs)| (Some(from_base(&cs.light, false)), Some(from_base(&cs.dark, true))))
            .unwrap_or((None, None)),
    }
}

/// Whether a system-scheme source has its colors file in the cache; enables its settings button.
pub fn source_available(scheme: &str) -> bool {
    #[cfg(target_os = "linux")]
    {
        sources(scheme).iter().any(|p| p.is_file())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = scheme;
        false
    }
}

/// The current source file and its modification time; changes mean new colors to pick up.
/// Linux only: system color sources exist nowhere else.
#[cfg(target_os = "linux")]
pub fn source_stamp(scheme: &str) -> Option<(PathBuf, std::time::SystemTime)> {
    let path = sources(scheme).into_iter().find(|p| p.is_file())?;
    let mtime = std::fs::metadata(&path).ok()?.modified().ok()?;
    Some((path, mtime))
}

/// The candidate colors files of a system-scheme source, in the user cache dir.
#[cfg(target_os = "linux")]
fn sources(scheme: &str) -> Vec<PathBuf> {
    let cache = dirs::cache_dir().unwrap_or_default();
    match scheme {
        "matugen" => vec![cache.join("matugen").join("colors.json")],
        // dux-shell (DankMaterialShell): the dir was renamed between versions.
        "dux" => vec![cache.join("DankMaterialShell").join("dms-colors.json"), cache.join("dms").join("dms-colors.json")],
        _ => vec![],
    }
}

/// Reads Material You colors from a system-scheme source's file (see `sources`).
///
/// Accepts the formats these tools produce: role-level nesting (`colors.<role>.dark/.light`,
/// as string or `{color: ".."}` — the `matugen --json` dump), mode-level nesting
/// (`colors.dark/light.<role>` — dux-shell's `dms-colors.json`), and a flat
/// `{"primary": "#..", ...}` template export. Shortcut: the file is only re-read
/// when the theme is re-applied, not watched.
#[cfg(target_os = "linux")]
fn cache_colors(scheme: &str) -> Option<SchemeColors> {
    let path = sources(scheme).into_iter().find(|p| p.is_file())?;
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let colors = v.get("colors").unwrap_or(&v);
    let base = |dark: bool| {
        let pick = |names: &[&str]| role(colors, names, dark);
        Some(SchemeBase {
            accent: pick(&["primary"])?,
            panel: pick(&["surface_container", "surface", "background"])?,
            chat: pick(&["surface", "background", "surface_dim"])?,
            bubble_in: pick(&["surface_container_high", "surface_container", "secondary_container"])?,
            bubble_out: pick(&["primary"])?,
            text: pick(&["on_surface", "on_background"])?,
        })
    };
    Some(SchemeColors { light: base(false)?, dark: base(true)? })
}

/// The first present role as `#RRGGBB` (`#RRGGBBAA` also accepted, alpha dropped).
///
/// `colors` may group by role (`primary.dark`) or by mode (`dark.primary`).
#[cfg(target_os = "linux")]
fn role(colors: &serde_json::Value, names: &[&str], dark: bool) -> Option<u32> {
    let hex = |v: &serde_json::Value| -> Option<u32> {
        let s = v.as_str().or_else(|| v.get("color").and_then(|c| c.as_str()))?.strip_prefix('#')?;
        match s.len() {
            6 => u32::from_str_radix(s, 16).ok(),
            8 => u32::from_str_radix(&s[..6], 16).ok(),
            _ => None,
        }
    };
    let mode = if dark { "dark" } else { "light" };
    for name in names {
        // Mode-level grouping: {"colors": {"dark": {"primary": "#.."}, "light": {...}}}
        if let Some(x) = colors.get(mode).and_then(|m| m.get(name)).and_then(|v| hex(v)) {
            return Some(x);
        }
        // Role-level grouping or a flat export: "primary" | {"color": ".."} | {"dark": "..", "light": ".."}
        let Some(e) = colors.get(name) else { continue };
        if let Some(x) = hex(e) {
            return Some(x);
        }
        if let Some(x) = e.get(mode).and_then(hex) {
            return Some(x);
        }
    }
    None
}

/// Re-skins gpui-component for the current mode; call after every theme change.
/// `scale` drives the rem size: gpui-component's Root sets rem = theme font size every frame.
/// Colour emoji font, set explicitly: on Linux the UI font (e.g. DejaVu Sans) has black-and-white
/// glyphs for the older emojis and would win over the colour font.
pub const EMOJI_FONT: &str = if cfg!(windows) {
    "Segoe UI Emoji"
} else if cfg!(target_os = "macos") {
    "Apple Color Emoji"
} else {
    "Noto Color Emoji"
};

pub fn apply(cx: &mut App, scheme: &str, custom: Option<SchemeColors>, scale: f32) {
    let (light, dark) = resolve(scheme, custom);
    cx.set_global(SchemeState { light, dark });
    let p = palette(cx);
    let on_accent = on_color(p.accent);
    Theme::update(cx, |t| {
        t.font_size = px(16. * scale);
        t.mono_font_size = px(13. * scale);
        let k = &mut t.colors;
        k.background = p.panel;
        k.foreground = p.text;
        k.border = p.border;
        k.input = p.border;
        k.ring = p.accent;
        k.caret = p.accent;
        k.link = p.accent;
        k.link_hover = p.accent_hover;
        k.primary = p.accent;
        k.primary_hover = p.accent_hover;
        k.primary_active = p.accent_hover;
        k.primary_foreground = on_accent;
        k.button_primary = p.accent;
        k.button_primary_hover = p.accent_hover;
        k.button_primary_active = p.accent_hover;
        k.button_primary_foreground = on_accent;
        k.accent = p.hover;
        k.accent_foreground = p.text;
        k.secondary = p.field;
        k.secondary_hover = p.hover;
        k.secondary_foreground = p.text;
        k.muted = p.field;
        k.muted_foreground = p.muted;
        k.popover = p.panel;
        k.popover_foreground = p.text;
        k.list_hover = p.hover;
        k.list_active = p.accent_soft;
        k.danger = p.danger;
        k.success = p.online;
        k.info = p.accent;
    });
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_BASE, PRESETS, Preset, c, from_base};
    use gpui_kit::Rgba;
    #[cfg(target_os = "linux")]
    use super::{SchemeBase, role};
    #[cfg(target_os = "linux")]
    use gpui_kit::rgb;

    /// Every preset must stay readable in both modes: bubble text contrasts with its bubble.
    #[test]
    fn presets_derive_readable_palettes() {
        for Preset(id, _, cs) in PRESETS {
            for dark in [false, true] {
                let p = from_base(if dark { &cs.dark } else { &cs.light }, dark);
                assert_eq!(p.dark, dark);
                let (b, t) = (Rgba::from(p.bubble_out), Rgba::from(p.bubble_out_text));
                assert!(
                    (b.r - t.r).abs() + (b.g - t.g).abs() + (b.b - t.b).abs() > 1.5,
                    "{id} dark={dark}: unreadable bubble"
                );
            }
        }
    }

    /// The base colors must land in their own palette slots (the editor maps indices the same way).
    #[test]
    fn default_base_order_is_picker_order() {
        for (dark, b) in [(false, DEFAULT_BASE.light), (true, DEFAULT_BASE.dark)] {
            let p = from_base(&b, dark);
            assert_eq!(Rgba::from(p.accent), Rgba::from(c(b.accent)));
            assert_eq!(Rgba::from(p.panel), Rgba::from(c(b.panel)));
            assert_eq!(Rgba::from(p.chat_top), Rgba::from(c(b.chat)));
            assert_eq!(Rgba::from(p.bubble_in), Rgba::from(c(b.bubble_in)));
            assert_eq!(Rgba::from(p.bubble_out), Rgba::from(c(b.bubble_out)));
            assert_eq!(Rgba::from(p.text), Rgba::from(c(b.text)));
        }
    }

    /// The scheme override plumbing: `apply` installs per-mode palettes, `palette` reads them back.
    #[test]
    fn apply_installs_scheme_override() {
        let cx = gpui_kit::TestAppContext::single();
        cx.update(|cx| {
            gpui_kit::init(cx);
            super::apply(cx, "emerald", None, 1.);
            let st = cx.try_global::<super::SchemeState>().expect("apply sets the scheme global");
            assert_eq!(Rgba::from(st.light.unwrap().accent), Rgba::from(c(PRESETS[0].2.light.accent)));
            assert!(st.dark.is_some(), "both modes are installed");

            super::apply(cx, "no-such-scheme", None, 1.);
            assert!(cx.try_global::<super::SchemeState>().unwrap().light.is_none(), "unknown ids fall back to the built-in palette");

            #[cfg(target_os = "linux")]
            if super::cache_colors("matugen").is_none() {
                super::apply(cx, "matugen", None, 1.);
                assert!(cx.try_global::<super::SchemeState>().unwrap().light.is_none(), "a missing colors file falls back to the built-in palette");
            }
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn source_formats_parse() {
        // A flat template export, the `matugen --json` dump (role-level nesting)
        // and dux-shell's dms-colors.json (mode-level nesting).
        let flat = r##"{"primary":"#1b6ef3","surface_container":"#171a1c","surface":"#14171a","surface_container_high":"#1d2124","on_surface":"#e2e2e9"}"##;
        let dump = r##"{"colors":{
            "primary":{"dark":{"color":"#a4c8ff"},"light":{"color":"#1b6ef3"}},
            "surface":{"dark":{"color":"#111318"},"light":{"color":"#f9f9ff"}},
            "surface_container_high":{"dark":{"color":"#1d2124"},"light":{"color":"#e9e9f2"}},
            "on_surface":{"dark":{"color":"#e2e2e9"},"light":{"color":"#1a1b21"}}}}"##;
        let dux = r##"{"colors":{"dark":{
            "primary":"#a4c8ff","surface":"#111318","surface_container":"#1a1e27",
            "surface_container_high":"#1d2124","on_surface":"#e2e2e9"},"light":{
            "primary":"#1b6ef3","surface":"#f9f9ff","surface_container":"#e9e9f2",
            "surface_container_high":"#e9e9f2","on_surface":"#1a1b21"}}}"##;
        for (json, dark, accent) in [(flat, false, 0x1b6ef3u32), (dump, true, 0xa4c8ff), (dux, true, 0xa4c8ff), (dux, false, 0x1b6ef3)] {
            let v: serde_json::Value = serde_json::from_str(json).unwrap();
            let colors = v.get("colors").unwrap_or(&v);
            let pick = |names: &[&str]| role(colors, names, dark);
            assert_eq!(pick(&["primary"]), Some(accent));
            let p = from_base(
                &SchemeBase {
                    accent: pick(&["primary"]).unwrap(),
                    panel: pick(&["surface_container", "surface", "background"]).unwrap(),
                    chat: pick(&["surface", "background", "surface_dim"]).unwrap(),
                    bubble_in: pick(&["surface_container_high", "surface_container", "secondary_container"]).unwrap(),
                    bubble_out: pick(&["primary"]).unwrap(),
                    text: pick(&["on_surface", "on_background"]).unwrap(),
                },
                dark,
            );
            let want: gpui_kit::Hsla = rgb(accent).into();
            assert_eq!((p.accent.h, p.accent.s), (want.h, want.s));
        }
    }
}
