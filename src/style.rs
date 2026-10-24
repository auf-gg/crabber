//! Design tokens: one palette per mode, also pushed into gpui-component so its
//! buttons, inputs and switches match the app.
use gpui_kit::component::{ActiveTheme as _, Theme};
use gpui_kit::*;

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

pub fn palette(cx: &App) -> Palette {
    if cx.theme().is_dark() { dark() } else { light() }
}

/// Re-skins gpui-component for the current mode; call after every theme change.
/// `scale` drives the rem size: gpui-component's Root sets rem = theme font size every frame.
pub fn apply(cx: &mut App, scale: f32) {
    let p = palette(cx);
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
        k.primary_foreground = c(0xFFFFFF);
        k.button_primary = p.accent;
        k.button_primary_hover = p.accent_hover;
        k.button_primary_active = p.accent_hover;
        k.button_primary_foreground = c(0xFFFFFF);
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
