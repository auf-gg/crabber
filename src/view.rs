//! Rendering. Telegram-like layout: chat list on the left, conversation on the right.
use std::cell::{Cell, RefCell};
use std::rc::Rc;

use chrono::{Datelike, Local, TimeZone};
use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, h_flex,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    switch::Switch,
    input::{Input, Paste, Textarea, TextareaState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use crabber::call::{CallCmd, CallState};
use crabber::store::{ChatRow, Msg};
use tokio_xmpp::parsers::presence::Show;

use crate::app::{App, MediaTab, Modal, Session, Viewer, ext, file_name, is_call, is_image, is_text};
use crate::style;
use crabber::e2e::DeviceInfo;

/// Design pixels in rems (16 px = 1 rem), so `Window::set_rem_size` scales the whole UI.
fn dp(x: f32) -> Rems {
    rems(x / 16.)
}

const MONTHS: [&str; 12] = ["января", "февраля", "марта", "апреля", "мая", "июня", "июля", "августа", "сентября", "октября", "ноября", "декабря"];

fn local(ts: i64) -> chrono::DateTime<Local> {
    Local.timestamp_millis_opt(ts).single().unwrap_or_else(Local::now)
}

fn short_time(ts: i64) -> String {
    let t = local(ts);
    if t.date_naive() == Local::now().date_naive() { t.format("%H:%M").to_string() } else { t.format("%d.%m.%y").to_string() }
}

fn day_label(ts: i64) -> String {
    let t = local(ts);
    let today = Local::now().date_naive();
    if t.date_naive() == today {
        "Сегодня".into()
    } else if Some(t.date_naive()) == today.pred_opt() {
        "Вчера".into()
    } else if t.year() == Local::now().year() {
        format!("{} {}", t.day(), MONTHS[t.month0() as usize])
    } else {
        format!("{} {} {}", t.day(), MONTHS[t.month0() as usize], t.year())
    }
}

/// Telegram palette: the colour is stable per JID.
const PALETTE: [u32; 7] = [0xE17076, 0xFAA774, 0xA695E7, 0x7BC862, 0x6EC9CB, 0x65AADD, 0xEE7AAE];

/// "bitcoin_casino" -> "BC", "Bob" -> "B".
pub fn initials(name: &str) -> String {
    name.split(|c: char| c.is_whitespace() || "_.-@".contains(c))
        .filter(|p| !p.is_empty())
        .take(2)
        .filter_map(|p| p.chars().next())
        .flat_map(char::to_uppercase)
        .collect()
}

fn avatar(sess: &Session, peer: &str, name: &str, size: f32) -> AnyElement {
    if let Some(image) = sess.avatars.get(peer) {
        // The image itself is rounded: overflow clipping in GPUI is rectangular, so a round parent would not crop it.
        return img(image.clone()).size(dp(size)).flex_none().rounded_full().object_fit(ObjectFit::Cover).into_any_element();
    }
    let base = div().size(dp(size)).flex_none().rounded_full();
    let hash = peer.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    base.flex()
        .items_center()
        .justify_center()
        .bg(rgb(PALETTE[hash as usize % PALETTE.len()]))
        .text_color(white())
        .text_size(dp(size * 0.4))
        .font_weight(FontWeight::SEMIBOLD)
        .child(initials(name))
        .into_any_element()
}

#[derive(Debug, PartialEq)]
pub enum Seg<'a> {
    Text(&'a str),
    Code { lang: &'a str, code: &'a str },
}

/// Splits a message on ``` fences; an unclosed fence stays plain text.
pub fn segments(body: &str) -> Vec<Seg<'_>> {
    let parts: Vec<&str> = body.split("```").collect();
    let fences = parts.len() - 1;
    let mut out = vec![];
    for (i, part) in parts.iter().enumerate() {
        let closed = i % 2 == 1 && (fences.is_multiple_of(2) || i < fences);
        if closed {
            let (lang, code) = match part.split_once('\n') {
                Some((first, rest)) if !first.trim().is_empty() && !first.trim().contains(' ') && first.trim().len() <= 20 => (first.trim(), rest),
                _ => ("", *part),
            };
            out.push(Seg::Code { lang, code: code.trim_matches('\n').trim_end() });
        } else {
            if i % 2 == 1 {
                out.push(Seg::Text("```"));
            }
            let text = part.trim_matches('\n');
            if !text.is_empty() {
                out.push(Seg::Text(text));
            }
        }
    }
    out
}

/// Mouse-selectable text (copied with Ctrl+C by gpui-base's `Root`).
/// Same as gpui-base's `SelectableText`, which cannot take highlights.
struct Selectable {
    id: ElementId,
    text: SharedString,
    styled: StyledText,
    /// Clickable URLs: byte range in `text` and the address it opens.
    links: Vec<(std::ops::Range<usize>, SharedString)>,
}

impl Selectable {
    fn new(id: impl Into<ElementId>, text: String, highlights: Vec<(std::ops::Range<usize>, HighlightStyle)>) -> Self {
        let text = SharedString::from(text);
        Self { id: id.into(), styled: StyledText::new(text.clone()).with_highlights(highlights), text, links: vec![] }
    }

    fn links(mut self, links: Vec<(std::ops::Range<usize>, SharedString)>) -> Self {
        self.links = links;
        self
    }

    /// Hand cursor over links; a click (press and release on the same link, nothing selected) opens it.
    fn paint_links(&self, id: &GlobalElementId, layout: &TextLayout, hitbox: &Hitbox, window: &mut Window) {
        let pressed = window.with_element_state(id, |s: Option<Rc<RefCell<Option<SharedString>>>>, _| {
            let s = s.unwrap_or_default();
            (s.clone(), s)
        });
        let link_at = Rc::new({
            let (layout, links) = (layout.clone(), self.links.clone());
            move |p: Point<Pixels>| layout.index_for_position(p).ok().and_then(|ix| links.iter().find(|(r, _)| r.contains(&ix)).map(|(_, u)| u.clone()))
        });
        let hovering = link_at(window.mouse_position()).is_some();
        if hovering {
            window.set_cursor_style(CursorStyle::PointingHand, hitbox);
        }
        let at = link_at.clone();
        window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, _| {
            if phase == DispatchPhase::Bubble && at(ev.position).is_some() != hovering {
                window.refresh();
            }
        });
        let (at, down, hb) = (link_at.clone(), pressed.clone(), hitbox.clone());
        window.on_mouse_event(move |ev: &MouseDownEvent, phase, window, _| {
            if phase == DispatchPhase::Bubble && ev.button == MouseButton::Left && hb.is_hovered(window) {
                *down.borrow_mut() = at(ev.position);
            }
        });
        let hb = hitbox.clone();
        window.on_mouse_event(move |ev: &MouseUpEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            let Some(url) = pressed.borrow_mut().take() else { return };
            if hb.is_hovered(window) && link_at(ev.position).as_ref() == Some(&url) && base::TextSelection::selected_text(window, cx).is_empty() {
                cx.open_url(&url);
            }
        });
    }
}

impl IntoElement for Selectable {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for Selectable {
    type RequestLayoutState = base::TextSelectionHandle;
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(&mut self, id: Option<&GlobalElementId>, iid: Option<&InspectorElementId>, window: &mut Window, cx: &mut gpui_kit::App) -> (LayoutId, Self::RequestLayoutState) {
        let handle = window.with_element_state(id.expect("Selectable has an id"), |h: Option<base::TextSelectionHandle>, window| {
            let h = h.unwrap_or_else(|| {
                let h = base::TextSelectionHandle::new(self.text.clone(), cx);
                // Nothing else redraws the window while dragging, so the highlight would lag until the next notify.
                h.refresh_window_on_change(window, cx).detach();
                h
            });
            (h.clone(), h)
        });
        (self.styled.request_layout(id, iid, window, cx).0, handle)
    }

    fn prepaint(&mut self, id: Option<&GlobalElementId>, iid: Option<&InspectorElementId>, bounds: Bounds<Pixels>, handle: &mut Self::RequestLayoutState, window: &mut Window, cx: &mut gpui_kit::App) -> Hitbox {
        self.styled.prepaint(id, iid, bounds, &mut (), window, cx);
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let reg = base::TextSelectionRegistration::new(hitbox.clone(), bounds).with_text_bounds(vec![bounds]).with_rendered_element(handle, window, cx);
        handle.register(reg, window, cx);
        hitbox
    }

    fn paint(&mut self, id: Option<&GlobalElementId>, iid: Option<&InspectorElementId>, bounds: Bounds<Pixels>, handle: &mut Self::RequestLayoutState, hitbox: &mut Hitbox, window: &mut Window, cx: &mut gpui_kit::App) {
        let layout = self.styled.layout().clone();
        if !self.links.is_empty() {
            self.paint_links(id.expect("Selectable has an id"), &layout, hitbox, window);
        }
        let before = base::TextSelection::selected_text(window, cx);
        let projection = handle.update_runs(&[base::TextSelectionRun::new(self.text.clone(), layout.clone(), bounds)], cx);
        if before != base::TextSelection::selected_text(window, cx) {
            window.refresh();
        }
        // The theme's blue selection vanishes on blue outgoing bubbles; the text colour shows on any bubble.
        let color = window.text_style().color.opacity(0.3);
        let (lh, lb) = (layout.line_height(), layout.bounds());
        for range in projection.ranges().iter().flatten() {
            let (Some(a), Some(b)) = (layout.position_for_index(range.start), layout.position_for_index(range.end)) else { continue };
            let quads = if a.y == b.y {
                vec![Bounds::from_corners(a, point(b.x, b.y + lh))]
            } else {
                vec![
                    Bounds::from_corners(a, point(lb.right(), a.y + lh)),
                    Bounds::from_corners(point(lb.left(), a.y + lh), point(lb.right(), b.y)),
                    Bounds::from_corners(point(lb.left(), b.y), point(b.x, b.y + lh)),
                ]
            };
            quads.into_iter().for_each(|q| window.paint_quad(fill(q, color)));
        }
        self.styled.paint(id, iid, bounds, &mut (), &mut (), window, cx);
    }
}

/// Ctrl+C with the composer focused: its own (empty) copy would win over text selected in a message.
fn copy_message_selection(compose: &Entity<TextareaState>, window: &mut Window, cx: &mut gpui_kit::App) -> bool {
    let text = base::TextSelection::selected_text(window, cx).trim().to_string();
    let take = !text.is_empty() && compose.read(cx).selected_value().is_empty();
    if take {
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }
    take
}

/// Plain text with `inline code` shaded and http(s) links underlined and clickable.
fn rich_text(id: ElementId, text: &str, code_bg: Hsla) -> AnyElement {
    let mut parts: Vec<&str> = text.split('`').collect();
    if parts.len() < 3 || parts.len().is_multiple_of(2) {
        parts = vec![text];
    }
    let mut plain = String::new();
    let mut highlights = vec![];
    let mut links = vec![];
    for (i, part) in parts.iter().enumerate() {
        let start = plain.len();
        plain.push_str(part);
        if i % 2 == 1 {
            if !part.is_empty() {
                highlights.push((start..plain.len(), HighlightStyle { background_color: Some(code_bg), ..Default::default() }));
            }
            continue;
        }
        for r in url_ranges(part) {
            let r = start + r.start..start + r.end;
            let underline = UnderlineStyle { thickness: px(1.), ..Default::default() };
            highlights.push((r.clone(), HighlightStyle { underline: Some(underline), ..Default::default() }));
            links.push((r.clone(), SharedString::from(plain[r].to_string())));
        }
    }
    Selectable::new(id, plain, highlights).links(links).into_any_element()
}

/// Byte ranges of http(s) URLs in `text`, without trailing punctuation.
fn url_ranges(text: &str) -> Vec<std::ops::Range<usize>> {
    text.split_whitespace()
        .filter_map(|w| {
            let at = w.find("https://").or_else(|| w.find("http://"))?;
            let url = w[at..].trim_end_matches([',', '.', ')', '!', '?', ';', ':', '"', '\'', '»']);
            let start = w.as_ptr() as usize - text.as_ptr() as usize + at;
            (url.len() > "https://".len()).then(|| start..start + url.len())
        })
        .collect()
}

/// Whether the chat is scrolled up from its last message. `ListState::is_scrolled_to_end`
/// answers "unknown" until every message has been measured, i.e. in any long chat.
fn below_bottom(list: &ListState) -> bool {
    let (n, viewport) = (list.item_count(), list.viewport_bounds());
    // Not laid out yet, or anchored to the end (tail following).
    if n == 0 || viewport.size.height <= px(0.) || list.logical_scroll_top().item_ix >= n {
        return false;
    }
    // Unmeasured last message: it is far below the visible (always measured) ones.
    list.bounds_for_item(n - 1).is_none_or(|b| b.bottom() > viewport.bottom() + px(1.))
}

/// Faint sea doodles over the chat gradient, like Telegram's patterned wallpapers.
/// Painted straight from one canvas: as ~400 laid-out icon elements it cost ~3.5 ms per frame.
fn doodles(color: Hsla) -> impl IntoElement {
    canvas(
        |_, _, _| (),
        move |bounds, _, window, cx| {
            let icons = [IconName::Shell, IconName::Fish, IconName::Anchor, IconName::Sailboat, IconName::Shrimp, IconName::Snail, IconName::Turtle];
            let unit = window.rem_size() / 16.; // one design pixel, follows the UI scale
            let tile = unit * 110.;
            let (cols, rows) = ((bounds.size.width / tile) as usize + 2, (bounds.size.height / (tile * 0.8)) as usize + 2);
            let scale = window.scale_factor();
            window.with_content_mask(Some(ContentMask { bounds }), |window| {
                for row in 0..rows {
                    for col in 0..cols {
                        // Cheap scramble so neighbours differ in icon, size, tilt and offset.
                        let h = row * 31 + col * 17;
                        let x = tile * (col as f32 + if row % 2 == 1 { 0.5 } else { 0. }) + unit * ((h % 5) as f32 * 6. - 30.);
                        let y = tile * 0.8 * row as f32 + unit * ((h % 3) as f32 * 8. - 20.);
                        let side = unit * (18. + (h % 4) as f32 * 3.);
                        let b = Bounds::new(bounds.origin + point(x, y), size(side, side));
                        let m = TransformationMatrix::unit()
                            .translate(b.center().scale(scale))
                            .rotate(radians((h % 7) as f32 * 0.5 - 1.5))
                            .translate(b.center().scale(-scale));
                        let _ = window.paint_svg(b, icons[h % icons.len()].path(), None, m, color, cx);
                    }
                }
            });
        },
    )
    .absolute()
    .inset_0()
}

/// White icon button for the dark viewer overlay (theme buttons would be dark-on-dark).
fn overlay_icon(id: &'static str, icon: IconName) -> Stateful<Div> {
    div().id(id).p_2().rounded_md().cursor_pointer().text_color(white()).hover(|d| d.bg(hsla(0., 0., 1., 0.15))).child(Icon::new(icon))
}

/// Stable per-nick colour for sender names in groups.
fn sender_color(nick: &str) -> Hsla {
    let hash = nick.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
    rgb(PALETTE[hash as usize % PALETTE.len()]).into()
}

/// ~80 chars of a message around the first match of `q` (already lowercase).
fn snippet(body: &str, q: &str) -> String {
    let chars: Vec<char> = body.replace('\n', " ").chars().collect();
    let lower: Vec<char> = chars.iter().map(|c| c.to_lowercase().next().unwrap_or(*c)).collect();
    let q: Vec<char> = q.trim().chars().collect();
    let pos = if q.is_empty() { 0 } else { lower.windows(q.len()).position(|w| w == q.as_slice()).unwrap_or(0) };
    let start = pos.saturating_sub(20);
    let text: String = chars[start..].iter().take(80).collect();
    if start > 0 { format!("…{text}") } else { text }
}

fn code_bg() -> Hsla {
    hsla(0., 0., 0.5, 0.18)
}

fn human_size(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} МБ", n as f64 / (1 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.0} КБ", n as f64 / (1 << 10) as f64),
        n => format!("{n} Б"),
    }
}

fn file_icon(name: &str) -> IconName {
    match ext(name).as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" | "svg" => IconName::FileImage,
        "zip" | "rar" | "7z" | "tar" | "gz" | "xz" => IconName::FileArchive,
        "mp3" | "wav" | "ogg" | "flac" | "m4a" | "opus" => IconName::FileMusic,
        "mp4" | "mkv" | "avi" | "mov" | "webm" => IconName::FileVideoCamera,
        "txt" | "md" | "log" | "csv" | "pdf" | "doc" | "docx" => IconName::FileText,
        _ if is_text(name) => IconName::FileCode,
        _ => IconName::File,
    }
}

fn icon_box(icon: IconName, size: f32) -> Div {
    div().size(dp(size)).flex_none().rounded_md().flex().items_center().justify_center().bg(code_bg()).child(Icon::new(icon))
}

fn preview(c: &ChatRow) -> String {
    let Some(m) = &c.last else { return String::new() };
    let text = if m.url.is_some() {
        if is_image(&m.body) { "Фото".to_string() } else { "Файл".to_string() }
    } else {
        m.body.replace("```", " ").split_whitespace().collect::<Vec<_>>().join(" ")
    };
    let prefix = if is_call(m) { String::new() } else if m.outgoing { "Вы: ".to_string() } else if c.is_room { format!("{}: ", m.sender) } else { String::new() };
    let full = prefix + &text;
    full.chars().take(60).collect()
}

impl Render for App {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let main = if self.sess.as_ref().is_some_and(|s| s.ever_online) {
            self.render_main(window, cx).into_any_element()
        } else {
            self.render_login(cx).into_any_element()
        };
        let p = style::palette(cx);
        let content = div()
            .flex_1()
            .min_h_0()
            .relative()
            .child(main)
            .when_some(self.render_modal(cx), |d, m| d.child(m))
            .when_some(self.render_settings(cx), |d, m| d.child(m))
            .when_some(self.render_media(cx), |d, m| d.child(m))
            .when_some(self.render_encryption(cx), |d, m| d.child(m))
            .when_some(self.render_pgp_import(cx), |d, m| d.child(m))
            .when_some(self.render_profile(cx), |d, m| d.child(m))
            .when_some(self.render_call(cx), |d, c| d.child(c))
            .when_some(self.render_viewer(cx), |d, v| d.child(v));
        v_flex()
            .size_full()
            .relative()
            .track_focus(&self.root_focus)
            .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                let k = &ev.keystroke;
                if k.key == "escape" && this.handle_escape(window, cx) {
                    cx.stop_propagation();
                } else if k.modifiers.control && matches!(k.key.as_str(), "=" | "+" | "-" | "0") {
                    match k.key.as_str() {
                        "0" => this.update_settings(window, cx, |st| st.ui_scale = 1.0),
                        "-" => this.step_scale(false, window, cx),
                        _ => this.step_scale(true, window, cx),
                    }
                    cx.stop_propagation();
                } else if cfg!(target_os = "linux") && k.key == "q" && (k.modifiers.platform || k.modifiers.control) {
                    // Linux: Super/Ctrl+Q hides to tray instead of quitting (quit is in the tray menu).
                    crate::desktop::hide_window(window);
                    cx.stop_propagation();
                }
            }))
            .bg(p.panel)
            .text_color(p.text)
            .when(cfg!(windows), |d| d.child(self.render_titlebar(window, cx)))
            .child(content)
            .when_some(self.render_msg_menu(window, cx), |d, m| d.child(m))
            .when_some(self.render_emoji_picker(window, cx), |d, m| d.child(m))
    }
}

impl App {
    /// Custom window chrome. The control buttons are Windows hit-test areas, so the system keeps
    /// snap layouts on Maximize, and Close goes through the normal close path (hide to tray).
    fn render_titlebar(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let p = style::palette(cx);
        let unread: u32 = self.sess.as_ref().map_or(0, |s| s.chats.iter().map(|c| c.unread).sum());
        let offline = self.sess.as_ref().is_some_and(|s| s.ever_online && !s.online);
        // Glyphs drawn here (Windows 11 style): the component icon set is not bundled in our assets.
        let glyph = p.text.opacity(0.85);
        let line = |w: f32, h: f32| div().w(dp(w)).h(px(h)).bg(glyph);
        let square = |sz: f32| div().size(dp(sz)).border_1().border_color(glyph).rounded(px(1.5));
        let control = |id: &'static str, glyph_el: AnyElement, area: WindowControlArea, close: bool| {
            div()
                .id(id)
                .w(dp(46.))
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(p.text)
                .when(close, |d| d.hover(|d| d.bg(rgb(0xE81123)).text_color(white())))
                .when(!close, |d| d.hover(|d| d.bg(p.hover)))
                .window_control_area(area)
                .child(glyph_el)
        };
        let maximized = window.is_maximized();
        let max_glyph = if maximized {
            div()
                .relative()
                .size(dp(10.))
                .child(square(8.).absolute().top(dp(0.)).right(dp(0.)))
                .child(square(8.).absolute().bottom(dp(0.)).left(dp(0.)).bg(p.panel))
                .into_any_element()
        } else {
            square(10.).into_any_element()
        };
        h_flex()
            .id("titlebar")
            .h(dp(36.))
            .flex_shrink_0()
            .bg(p.panel)
            .border_b_1()
            .border_color(p.border)
            .child(
                h_flex()
                    .id("titlebar-drag")
                    .flex_1()
                    .h_full()
                    .pl_3()
                    .gap_2()
                    .items_center()
                    .window_control_area(WindowControlArea::Drag)
                    .child(img(self.logo.clone()).size(dp(20.)))
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Crabber"))
                    .when(unread > 0, |d| {
                        d.child(div().px_1p5().h(dp(18.)).rounded_full().flex().items_center().bg(p.accent).text_color(white()).text_xs().font_weight(FontWeight::SEMIBOLD).child(unread.to_string()))
                    })
                    .when(offline, |d| d.child(div().text_xs().text_color(p.muted).child("· соединение...")))
                    .when(self.settings.ui_scale != 1.0, |d| d.child(div().text_xs().text_color(p.muted).child(format!("· {:.0}%", self.settings.ui_scale * 100.)))),
            )
            .child(control("win-min", line(10., 1.).into_any_element(), WindowControlArea::Min, false))
            .child(control("win-max", max_glyph, WindowControlArea::Max, false))
            .child(control("win-close", Icon::new(IconName::X).size(dp(15.)).into_any_element(), WindowControlArea::Close, true))
            .into_any_element()
    }

    fn render_login(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = style::palette(cx);
        let bg = if p.dark {
            linear_gradient(160., linear_color_stop(p.chat_top, 0.), linear_color_stop(p.accent_soft, 1.))
        } else {
            linear_gradient(160., linear_color_stop(rgb(0x3390EC), 0.), linear_color_stop(rgb(0x7BC4F7), 1.))
        };
        v_flex().size_full().items_center().justify_center().bg(bg).child(
            v_flex()
                .w(dp(380.))
                .gap_3()
                .p_8()
                .rounded(dp(20.))
                .bg(p.panel)
                .shadow_2xl()
                .child(
                    v_flex()
                        .items_center()
                        .gap_2()
                        .pb_2()
                        .child(img(self.logo.clone()).size(dp(88.)))
                        .child(div().text_2xl().font_weight(FontWeight::BOLD).child("Crabber"))
                        .child(div().text_sm().text_color(p.muted).child("Вход в XMPP-аккаунт")),
                )
                .child(Input::new(&self.login_jid))
                .child(Input::new(&self.login_pass).mask_toggle())
                .child(Input::new(&self.login_host))
                .child(
                    Checkbox::new("insecure")
                        .label("Доверять самоподписанному сертификату")
                        .checked(self.insecure)
                        .on_click(cx.listener(|this, v: &bool, _, cx| {
                            this.insecure = *v;
                            cx.notify();
                        })),
                )
                .child(
                    Checkbox::new("remember")
                        .label("Запомнить меня")
                        .checked(self.remember)
                        .on_click(cx.listener(|this, v: &bool, _, cx| {
                            this.remember = *v;
                            cx.notify();
                        })),
                )
                .child(
                    Button::new("login")
                        .primary()
                        .large()
                        .w_full()
                        .label("Войти")
                        .loading(self.connecting)
                        .on_click(cx.listener(|this, _, _, cx| this.login(cx))),
                )
                .when(!self.login_error.is_empty(), |d| {
                    d.child(div().p_2().rounded_md().bg(p.danger.opacity(0.1)).text_sm().text_color(p.danger).child(self.login_error.clone()))
                }),
        )
    }

    fn render_main(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .size_full()
            .child(self.render_sidebar(cx))
            .child(self.render_conversation(window, cx))
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = style::palette(cx);
        let sess = self.sess.as_ref().unwrap();
        let query = self.search.read(cx).value().to_lowercase();
        let (show_label, show_color) = match sess.show {
            None => ("в сети", p.online),
            Some(Show::Away) => ("отошёл", rgb(0xF5A623).into()),
            Some(Show::Dnd) => ("не беспокоить", p.danger),
            _ => ("в сети", p.online),
        };
        let me_name = sess.localpart();

        let header = h_flex()
            .px_3()
            .pt_3()
            .pb_2()
            .gap_3()
            .items_center()
            .child(
                div()
                    .id("my-avatar")
                    .cursor_pointer()
                    .child(avatar(sess, &sess.me, &me_name, 42.))
                    .on_click(cx.listener(|this, _, _, cx| this.change_avatar(cx))),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().text_base().font_weight(FontWeight::SEMIBOLD).truncate().child(me_name))
                    .child(
                        h_flex()
                            .id("show")
                            .gap_1p5()
                            .items_center()
                            .cursor_pointer()
                            .text_xs()
                            .text_color(p.muted)
                            .child(div().size(dp(8.)).rounded_full().bg(if sess.online { show_color } else { p.muted }))
                            .child(if sess.online { show_label } else { "нет связи" })
                            .on_click(cx.listener(|this, _, _, cx| this.cycle_show(cx))),
                    ),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(Button::new("add-contact").ghost().small().icon(IconName::UserPlus).tooltip("Добавить контакт").on_click(cx.listener(|this, _, w, cx| this.open_modal(Modal::AddContact, w, cx))))
                    .child(Button::new("new-group").ghost().small().icon(IconName::Users).tooltip("Новая группа").on_click(cx.listener(|this, _, w, cx| this.open_modal(Modal::NewGroup, w, cx))))
                    .child(Button::new("join-group").ghost().small().icon(IconName::Hash).tooltip("Войти в группу по адресу").on_click(cx.listener(|this, _, w, cx| this.open_modal(Modal::JoinGroup, w, cx))))
                    .child(Button::new("settings").ghost().small().icon(IconName::Settings).tooltip("Настройки").on_click(cx.listener(|this, _, _, cx| this.open_settings(cx)))),
            );

        let search = div().mx_3().mb_2().px_3().py_0p5().rounded_full().bg(p.field).child(
            Input::new(&self.search).appearance(false).cleanable(true).prefix(Icon::new(IconName::Search).text_color(p.muted)),
        );

        let requests = sess.requests.iter().map(|jid| {
            let (a, d) = (jid.clone(), jid.clone());
            v_flex()
                .mx_2()
                .mb_2()
                .p_3()
                .gap_2()
                .rounded_xl()
                .bg(p.accent_soft)
                .child(div().text_sm().child(format!("{jid} хочет добавить вас в контакты")))
                .child(
                    h_flex()
                        .gap_2()
                        .child(Button::new(SharedString::from(format!("acc-{jid}"))).primary().small().label("Принять").on_click(cx.listener(move |this, _, _, cx| this.answer_request(a.clone(), true, cx))))
                        .child(Button::new(SharedString::from(format!("rej-{jid}"))).ghost().small().label("Отклонить").on_click(cx.listener(move |this, _, _, cx| this.answer_request(d.clone(), false, cx)))),
                )
        });

        let white_soft = hsla(0., 0., 1., 0.8);
        let scoped = !query.is_empty() && sess.selected.is_some() && !self.search_all;
        let visible: Vec<&crabber::store::ChatRow> = sess
            .chats
            .iter()
            .filter(|_| !scoped)
            .filter(|c| query.is_empty() || c.peer.to_lowercase().contains(&query) || c.name.to_lowercase().contains(&query))
            .collect();
        let count = visible.len();
        let selected_ix = visible.iter().position(|c| sess.selected.as_deref() == Some(&c.peer));
        let chats = visible
            .into_iter()
            .enumerate()
            .map(|(i, c)| {
                // No line next to the highlighted row, like Telegram.
                let divider = i + 1 < count && selected_ix != Some(i) && selected_ix != Some(i + 1);
                (c, divider)
            })
            .map(|(c, divider)| {
                let name = sess.display_name(&c.peer);
                let active = sess.selected.as_deref() == Some(&c.peer);
                let online = sess.presence.contains(&c.peer);
                let typing = sess.typing.get(&c.peer).is_some_and(|(_, at)| at.elapsed().as_secs() < 30);
                let peer = c.peer.clone();
                let sub = if typing { "печатает...".to_string() } else { preview(c) };
                let locked = c.last.as_ref().is_some_and(|m| m.enc > 0);
                let (title_c, sub_c, time_c) = if active { (white(), white_soft, white_soft) } else { (p.text, if typing { p.accent } else { p.muted }, p.muted) };
                let row = h_flex()
                    .id(SharedString::from(format!("chat-{}", c.peer)))
                    .mx_2()
                    .px_2()
                    .py_2()
                    .gap_3()
                    .items_center()
                    .rounded_xl()
                    .cursor_pointer()
                    .when(active, |d| d.bg(p.accent))
                    .when(!active, |d| d.hover(|d| d.bg(p.hover)))
                    .child(
                        div()
                            .relative()
                            .child(avatar(sess, &c.peer, &name, 50.))
                            .when(online, |d| {
                                d.child(div().absolute().bottom(dp(1.)).right(dp(1.)).size(dp(13.)).rounded_full().border_2().border_color(if active { p.accent } else { p.panel }).bg(p.online))
                            }),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap_0p5()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .child(div().flex_1().min_w_0().truncate().font_weight(FontWeight::SEMIBOLD).text_color(title_c).child(name))
                                    .child(div().text_xs().text_color(time_c).child(c.last.as_ref().map(|m| short_time(m.ts)).unwrap_or_default())),
                            )
                            .child(
                                h_flex()
                                    .gap_1()
                                    .items_center()
                                    .when(locked && !typing, |d| d.child(Icon::new(IconName::Lock).xsmall().text_color(sub_c)))
                                    .child(div().flex_1().min_w_0().truncate().text_sm().text_color(sub_c).child(sub))
                                    .when(c.unread > 0, |d| {
                                        d.child(
                                            div()
                                                .min_w(dp(22.))
                                                .h(dp(22.))
                                                .px_1p5()
                                                .rounded_full()
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .text_xs()
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .bg(if active { white() } else { p.accent })
                                                .text_color(if active { p.accent } else { white() })
                                                .child(c.unread.to_string()),
                                        )
                                    }),
                            ),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| this.select(peer.clone(), window, cx)));
                v_flex().child(row).when(divider, |d| d.child(div().h(dp(1.)).ml(dp(78.)).mr_3().bg(p.divider)))
            });

        let scope_bar = (!query.is_empty() && sess.selected.is_some()).then(|| {
            let chat_name = sess.selected.as_ref().map(|s| sess.display_name(s)).unwrap_or_default();
            let all = self.search_all;
            h_flex()
                .mx_3()
                .mb_2()
                .gap_1()
                .child(
                    Button::new("scope-chat")
                        .small()
                        .label(format!("В чате «{chat_name}»"))
                        .when(!all, |b| b.primary())
                        .when(all, |b| b.ghost())
                        .on_click(cx.listener(|this, _, _, cx| this.set_search_all(false, cx))),
                )
                .child(
                    Button::new("scope-all")
                        .small()
                        .label("Во всех чатах")
                        .when(all, |b| b.primary())
                        .when(!all, |b| b.ghost())
                        .on_click(cx.listener(|this, _, _, cx| this.set_search_all(true, cx))),
                )
        });

        let hits = (!self.search_results.is_empty()).then(|| {
            v_flex()
                .child(div().px_4().pt_3().pb_1().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(p.muted).child(if scoped { "В ЭТОМ ЧАТЕ" } else { "СООБЩЕНИЯ" }))
                .children(self.search_results.iter().enumerate().map(|(i, (peer, m))| {
                    let name = sess.display_name(peer);
                    let (pr, id, ts) = (peer.clone(), m.msg_id.clone(), m.ts);
                    let snippet = snippet(&m.body, &query);
                    h_flex()
                        .id(SharedString::from(format!("hit-{i}")))
                        .mx_2()
                        .px_2()
                        .py_2()
                        .gap_3()
                        .items_center()
                        .rounded_xl()
                        .cursor_pointer()
                        .hover(|d| d.bg(p.hover))
                        .child(avatar(sess, peer, &name, 40.))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(h_flex().gap_2().child(div().flex_1().truncate().text_sm().font_weight(FontWeight::SEMIBOLD).child(name)).child(div().text_xs().text_color(p.muted).child(short_time(m.ts))))
                                .child(div().text_sm().text_color(p.muted).truncate().child(snippet)),
                        )
                        .on_click(cx.listener(move |this, _, w, cx| this.open_message(pr.clone(), id.clone(), ts, w, cx)))
                }))
        });

        v_flex()
            .w(dp(340.))
            .h_full()
            .flex_shrink_0()
            .bg(p.panel)
            .border_r_1()
            .border_color(p.border)
            .child(header)
            .child(search)
            .children(scope_bar)
            .children(requests)
            .child(v_flex().id("chats").flex_1().gap_0p5().overflow_y_scroll().children(chats).children(hits))
            .when(sess.chats.is_empty() && sess.requests.is_empty(), |d| {
                d.child(
                    v_flex()
                        .p_6()
                        .gap_2()
                        .items_center()
                        .text_center()
                        .child(Icon::new(IconName::Users).size(dp(40.)).text_color(p.muted))
                        .child(div().text_sm().text_color(p.muted).child("Пока нет чатов. Добавьте контакт или создайте группу.")),
                )
            })
    }

    fn render_conversation(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = style::palette(cx);
        let wallpaper = linear_gradient(180., linear_color_stop(p.chat_top, 0.), linear_color_stop(p.chat_bottom, 1.));
        let pattern = p.text.opacity(if p.dark { 0.05 } else { 0.07 });
        let sess = self.sess.as_ref().unwrap();
        let Some(peer) = sess.selected.clone() else {
            return v_flex()
                .flex_1()
                .h_full()
                .items_center()
                .justify_center()
                .gap_3()
                .relative()
                .bg(wallpaper)
                .child(doodles(pattern))
                .child(div().px_4().py_1p5().rounded_full().bg(p.chip).text_sm().text_color(p.chip_text).child("Выберите чат, чтобы начать общение"))
                .when(!sess.online, |d| d.child(div().px_3().py_1().rounded_full().bg(p.chip).text_xs().text_color(p.chip_text).child(sess.status.clone())))
                .into_any_element();
        };
        let chat = sess.chat(&peer).cloned().unwrap_or_default();
        let name = sess.display_name(&peer);
        let typing = sess.typing.get(&peer).filter(|(_, at)| at.elapsed().as_secs() < 30).map(|(who, _)| who.clone());
        let online = sess.presence.contains(&peer);
        let (subtitle, highlight) = if let Some(who) = typing {
            (if chat.is_room { format!("{who} печатает...") } else { "печатает...".into() }, true)
        } else if chat.is_room {
            (match sess.occupants.get(&peer) {
                Some(n) => format!("{n} в сети"),
                None => "группа".into(),
            }, false)
        } else if online {
            ("в сети".into(), true)
        } else if chat.in_roster && (chat.subscription == "none" || chat.subscription == "from") {
            ("ожидает подтверждения".into(), false)
        } else {
            ("не в сети".into(), false)
        };

        let header = h_flex()
            .h(dp(60.))
            .flex_shrink_0()
            .px_4()
            .gap_3()
            .items_center()
            .bg(p.panel)
            .border_b_1()
            .border_color(p.border)
            .child({
                let pr = peer.clone();
                h_flex()
                    .id("peer-info")
                    .flex_1()
                    .min_w_0()
                    .gap_3()
                    .items_center()
                    .cursor_pointer()
                    .child(avatar(sess, &peer, &name, 42.))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_base().font_weight(FontWeight::SEMIBOLD).truncate().child(name))
                            .child(div().text_sm().text_color(if highlight { p.accent } else { p.muted }).child(subtitle)),
                    )
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.modal = Some(Modal::Profile { peer: pr.clone() });
                        cx.notify();
                    }))
            })
            .when(!chat.is_room, |d| {
                let to = peer.clone();
                d.child(
                    Button::new("call")
                        .ghost()
                        .icon(IconName::Phone)
                        .tooltip("Аудиозвонок")
                        .disabled(sess.call.is_some() || !sess.online)
                        .on_click(cx.listener(move |this, _, _, cx| this.call(CallCmd::Start { to: to.clone() }, cx))),
                )
            })
            .when(!chat.is_room, |d| {
                let pr = peer.clone();
                let secure = matches!(chat.enc.as_str(), "" | "auto" | "omemo") && sess.e2e_devices.get(&peer).is_none_or(|d| !d.is_empty());
                d.child(
                    Button::new("e2e")
                        .ghost()
                        .icon(if secure || chat.enc == "pgp" { IconName::Lock } else { IconName::LockOpen })
                        .tooltip("Шифрование")
                        .on_click(cx.listener(move |this, _, _, cx| this.open_encryption(pr.clone(), cx))),
                )
            })
            .child({
                let pr = peer.clone();
                Button::new("media").ghost().icon(IconName::Image).tooltip("Медиа, файлы и ссылки").on_click(cx.listener(move |this, _, _, cx| this.open_media(pr.clone(), cx)))
            })
            .when(chat.is_room, |d| {
                let room = peer.clone();
                d.child(Button::new("invite").ghost().icon(IconName::UserPlus).tooltip("Пригласить").on_click(cx.listener(move |this, _, w, cx| this.open_modal(Modal::Invite { room: room.clone() }, w, cx))))
            });

        let is_room = chat.is_room;
        let messages = list(
            self.messages.clone(),
            cx.processor(move |this: &mut App, ix: usize, _window, cx| this.render_message(ix, is_room, cx)),
        )
        .size_full()
        .pb_2();

        let pending = (!sess.pending.is_empty()).then(|| {
            h_flex().flex_wrap().gap_2().px_4().pt_3().bg(p.panel).border_t_1().border_color(p.border).children(sess.pending.iter().enumerate().map(|(ix, f)| {
                let name = f.file_name().and_then(|n| n.to_str()).unwrap_or("файл").to_string();
                let size = std::fs::metadata(f).map(|m| human_size(m.len())).unwrap_or_default();
                let thumb = if is_image(&name) {
                    img(f.clone()).size(dp(48.)).rounded_lg().object_fit(ObjectFit::Cover).into_any_element()
                } else {
                    icon_box(file_icon(&name), 48.).into_any_element()
                };
                h_flex()
                    .gap_2()
                    .p_1()
                    .pr_2()
                    .rounded_xl()
                    .bg(p.field)
                    .child(thumb)
                    .child(v_flex().max_w(dp(160.)).child(div().text_sm().truncate().child(name)).child(div().text_xs().text_color(p.muted).child(size)))
                    .child(Button::new(SharedString::from(format!("rm-{ix}"))).ghost().xsmall().icon(IconName::X).tooltip("Убрать").on_click(cx.listener(move |this, _, _, cx| this.remove_pending(ix, cx))))
            }))
        });

        let editing = sess.editing.as_ref().and_then(|id| sess.history.iter().find(|m| m.msg_id == *id)).map(|m| {
            h_flex()
                .flex_shrink_0()
                .px_4()
                .pt_2()
                .gap_3()
                .items_center()
                .bg(p.panel)
                .border_t_1()
                .border_color(p.border)
                .child(Icon::new(IconName::Pencil).small().text_color(p.accent))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(p.accent).child("Изменение сообщения"))
                        .child(div().text_sm().text_color(p.muted).truncate().child(m.body.replace('\n', " "))),
                )
                .child(Button::new("edit-cancel").ghost().xsmall().icon(IconName::X).tooltip("Отменить").on_click(cx.listener(|this, _, w, cx| this.cancel_edit(w, cx))))
        });

        let composer = h_flex()
            .flex_shrink_0()
            .px_3()
            .py_2()
            .gap_2()
            .items_end()
            .bg(p.panel)
            .when(pending.is_none() && editing.is_none(), |d| d.border_t_1().border_color(p.border))
            .child(Button::new("attach").ghost().icon(IconName::Paperclip).tooltip("Прикрепить файл").on_click(cx.listener(|this, _, _, cx| this.attach(cx))))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .px_3()
                    .py_1()
                    .rounded(dp(20.))
                    .bg(p.field)
                    .capture_action(cx.listener(|this, _: &gpui_kit::component::input::Copy, window, cx| {
                        if copy_message_selection(&this.compose, window, cx) {
                            cx.stop_propagation();
                        }
                    }))
                    .capture_action(cx.listener(|this, _: &Paste, _, cx| {
                        if this.paste_attachments(cx) {
                            cx.stop_propagation();
                        }
                    }))
                    .child(Textarea::new(&self.compose).appearance(false)),
            )
            .child(
                div()
                    .id("send")
                    .size(dp(40.))
                    .flex_shrink_0()
                    .rounded_full()
                    .bg(p.accent)
                    .hover(|d| d.bg(p.accent_hover))
                    .cursor_pointer()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(Icon::new(IconName::SendHorizontal).text_color(white()))
                    .on_click(cx.listener(|this, _, window, cx| this.send_text(window, cx))),
            );

        v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .child(header)
            .when(!sess.online, |d| d.child(div().px_4().py_1().text_xs().bg(p.danger.opacity(0.12)).text_color(p.danger).child(sess.status.clone())))
            .child(div().flex_1().min_h_0().relative().bg(wallpaper).child(doodles(pattern)).child(messages).when(below_bottom(&self.messages), |d| {
                d.child(
                    div()
                        .id("to-bottom")
                        .absolute()
                        // Without it the click also reaches the picture underneath and opens the viewer.
                        .occlude()
                        .right_4()
                        .bottom_4()
                        .size(dp(42.))
                        .rounded_full()
                        .bg(p.panel)
                        .shadow_md()
                        .cursor_pointer()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_color(p.muted)
                        .hover(|d| d.text_color(p.accent))
                        .child(Icon::new(IconName::ChevronDown).size(dp(22.)))
                        // Tail mode jumps to the end and keeps following new messages.
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.messages.set_follow_mode(FollowMode::Tail);
                            cx.notify();
                        })),
                )
            }))
            .children(pending)
            .children(editing)
            .child(composer)
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| this.add_pending(paths.0.iter().cloned(), cx)))
            .drag_over::<ExternalPaths>(move |s, _, _, _| s.opacity(0.85))
            .into_any_element()
    }

    fn render_message(&mut self, ix: usize, is_room: bool, cx: &mut Context<Self>) -> AnyElement {
        let p = style::palette(cx);
        let Some(sess) = self.sess.as_ref() else { return div().into_any_element() };
        let Some(m) = sess.history.get(ix).cloned() else { return div().into_any_element() };
        let flash = sess.flash.as_ref().is_some_and(|(id, _)| *id == m.msg_id);
        let prev = ix.checked_sub(1).and_then(|i| sess.history.get(i));
        let next = sess.history.get(ix + 1);
        let new_day = prev.is_none_or(|q| local(q.ts).date_naive() != local(m.ts).date_naive());
        // Consecutive messages of one sender within 3 minutes form a group: one tail, tight spacing.
        let same = |a: &Msg, b: &Msg| a.outgoing == b.outgoing && a.sender == b.sender && (a.ts - b.ts).abs() < 180_000 && local(a.ts).date_naive() == local(b.ts).date_naive();
        let first_in_group = new_day || prev.is_none_or(|q| !same(q, &m));
        let last_in_group = next.is_none_or(|n| !same(n, &m));
        let show_sender = is_room && !m.outgoing && first_in_group;
        let (bg, fg, meta_c) = if m.outgoing { (p.bubble_out, p.bubble_out_text, p.meta_out) } else { (p.bubble_in, p.bubble_in_text, p.muted) };
        let code = hsla(0., 0., if p.dark || m.outgoing { 0. } else { 0.5 }, 0.18);

        // (emoji, count, ours) in first-seen order.
        let me = sess.selected.as_deref().map(|p| sess.my_sender(p)).unwrap_or_default();
        let mut chips: Vec<(String, usize, bool)> = vec![];
        for (who, e) in sess.reactions.get(&m.msg_id).into_iter().flatten() {
            match chips.iter_mut().find(|c| c.0 == *e) {
                Some(c) => {
                    c.1 += 1;
                    c.2 |= *who == me;
                }
                None => chips.push((e.clone(), 1, *who == me)),
            }
        }
        let reactions = (!chips.is_empty()).then(|| {
            h_flex().flex_wrap().gap_1().pt_0p5().children(chips.into_iter().map(|(e, n, mine)| {
                let (id, emoji) = (m.msg_id.clone(), e.clone());
                div()
                    .id(SharedString::from(format!("re-{}-{e}", m.msg_id)))
                    .px_1p5()
                    .rounded_full()
                    .text_sm()
                    .cursor_pointer()
                    .bg(if mine { p.accent.opacity(0.3) } else { code })
                    .child(format!("{e} {n}"))
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_reaction(&id, &emoji, cx)))
            }))
        });
        let content = match &m.url {
            _ if is_call(&m) => h_flex().gap_2().items_center().child(Icon::new(IconName::Phone).small()).child(m.body.clone()).into_any_element(),
            Some(url) => self.render_attachment(url, &m, cx),
            None => v_flex()
                .gap_1()
                .children(segments(&m.body).into_iter().enumerate().map(|(i, seg)| match seg {
                    Seg::Text(text) => rich_text(SharedString::from(format!("txt-{}-{i}", m.msg_id)).into(), text, code),
                    Seg::Code { lang, code: src } => {
                        let copy = src.to_string();
                        v_flex()
                            .min_w(dp(260.))
                            .rounded_lg()
                            .bg(code)
                            .child(
                                h_flex()
                                    .pl_3()
                                    .pr_1()
                                    .pt_0p5()
                                    .justify_between()
                                    .items_center()
                                    .text_xs()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .opacity(0.8)
                                    .child(if lang.is_empty() { "код".to_string() } else { lang.to_string() })
                                    .child(
                                        // Plain icon so it takes the bubble's text colour (a Button would not on dark bubbles).
                                        div()
                                            .id(SharedString::from(format!("copy-{}-{i}", m.msg_id)))
                                            .p_1()
                                            .rounded_sm()
                                            .cursor_pointer()
                                            .hover(|d| d.bg(code))
                                            .child(Icon::new(IconName::Copy).xsmall())
                                            .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))),
                                    ),
                            )
                            .child(div().px_3().pb_2().font_family("Consolas").text_sm().child(Selectable::new(SharedString::from(format!("src-{}-{i}", m.msg_id)), src.replace('\t', "    "), vec![])))
                            .into_any_element()
                    }
                }))
                .into_any_element(),
        };
        let ticks = (m.outgoing && !is_call(&m)).then(|| match m.status {
            -1 => div().text_color(p.danger).font_weight(FontWeight::BOLD).child("!").into_any_element(),
            0 => Icon::new(IconName::Check).xsmall().into_any_element(),
            1 => Icon::new(IconName::CheckCheck).xsmall().opacity(0.7).into_any_element(),
            _ => Icon::new(IconName::CheckCheck).xsmall().into_any_element(),
        });
        let meta = h_flex()
            .gap_1()
            .justify_end()
            .items_center()
            .text_xs()
            .text_color(meta_c)
            .when(m.edited, |d| d.child("изменено"))
            .when(m.enc > 0, |d| d.child(Icon::new(IconName::Lock).xsmall()))
            .child(local(m.ts).format("%H:%M").to_string())
            .children(ticks);

        let radius = dp(16.);
        let tail = dp(4.);
        let menu_msg = m.clone();
        let bubble = v_flex()
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                    this.msg_menu = Some((menu_msg.clone(), ev.position));
                    cx.notify();
                }),
            )
            .when(!is_call(&m), |d| {
                let id = m.msg_id.clone();
                d.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                        if ev.click_count == 2 {
                            this.add_reaction(&id, "❤️", cx);
                        }
                    }),
                )
            })
            .max_w(relative(0.68))
            .min_w(dp(72.))
            .px_3()
            .pt_1p5()
            .pb_1()
            .gap_0p5()
            .bg(bg)
            .text_color(fg)
            .rounded(radius)
            .when(m.outgoing && last_in_group, |d| d.rounded_br(tail))
            .when(!m.outgoing && last_in_group, |d| d.rounded_bl(tail))
            .when(!p.dark, |d| d.shadow_sm())
            .when(show_sender, |b| b.child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(sender_color(&m.sender)).child(m.sender.clone())))
            .child(content)
            .children(reactions)
            .child(meta);

        v_flex()
            .w_full()
            .px_4()
            .pt(dp(if first_in_group { 6. } else { 2. }))
            .when(flash, |d| d.bg(p.accent.opacity(0.18)))
            .when(new_day, |d| {
                d.child(h_flex().justify_center().py_2().child(div().px_3().py_1().rounded_full().text_xs().font_weight(FontWeight::SEMIBOLD).bg(p.chip).text_color(p.chip_text).child(day_label(m.ts))))
            })
            .child(h_flex().w_full().when(m.outgoing, |d| d.justify_end()).child(bubble))
            .into_any_element()
    }

    fn render_attachment(&mut self, url: &str, m: &Msg, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        let sess = self.sess.as_ref().unwrap();
        let path = sess.store.file_for(url).unwrap_or_default();
        let downloaded = path.is_file();
        let failed = sess.failed.contains(url);
        let name = file_name(url);
        let u = url.to_string();
        let open = cx.listener(move |this, _, window, cx| this.open_attachment(u.clone(), window, cx));
        if is_image(url) {
            if !downloaded {
                if failed {
                    return div()
                        .id(SharedString::from(format!("img-{}", m.msg_id)))
                        .cursor_pointer()
                        .text_sm()
                        .opacity(0.7)
                        .child("Изображение недоступно · нажмите, чтобы повторить")
                        .on_click(open)
                        .into_any_element();
                }
                let requested = self.sess.as_ref().is_some_and(|s| s.downloads.contains(url));
                if !self.settings.auto_download && !requested {
                    return div()
                        .id(SharedString::from(format!("img-{}", m.msg_id)))
                        .cursor_pointer()
                        .child(h_flex().gap_2().items_center().child(icon_box(IconName::Download, 40.)).child(div().text_sm().child("Фото · нажмите, чтобы загрузить")))
                        .on_click(open)
                        .into_any_element();
                }
                self.request_download(url);
                return div().text_sm().opacity(0.7).child("Загрузка изображения...").into_any_element();
            }
            // Fit into 320x320 keeping the aspect ratio, so the bubble hugs the picture.
            let (w, h) = *self.img_dims.entry(path.clone()).or_insert_with(|| image::image_dimensions(&path).map(|(w, h)| (w as f32, h as f32)).unwrap_or((320., 240.)));
            let scale = (320. / w).min(320. / h).min(1.);
            return div()
                .id(SharedString::from(format!("img-{}", m.msg_id)))
                .cursor_pointer()
                .child(img(path).w(dp(w * scale)).h(dp(h * scale)).rounded_md())
                .on_click(open)
                .into_any_element();
        }
        // Text files are small and previewed inline, so fetch them like images.
        if is_text(url) && !downloaded && self.settings.auto_download {
            self.request_download(url);
        }
        let subtitle = if downloaded {
            let size = std::fs::metadata(&path).map(|m| human_size(m.len())).unwrap_or_default();
            if is_text(url) { format!("{size} · открыть") } else { format!("{size} · показать в папке") }
        } else if failed {
            "файл недоступен · повторить".to_string()
        } else {
            "нажмите, чтобы скачать".to_string()
        };
        let preview = (downloaded && is_text(url)).then(|| {
            let bytes = std::fs::read(&path).map(|b| b[..b.len().min(4096)].to_vec()).unwrap_or_default();
            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<&str> = text.lines().take(8).collect();
            div().w_full().px_3().py_2().rounded_md().bg(code_bg()).font_family("Consolas").text_xs().child(lines.join("\n").replace('\t', "    "))
        });
        v_flex()
            .id(SharedString::from(format!("file-{}", m.msg_id)))
            .gap_2()
            .min_w(dp(220.))
            .max_w(dp(420.))
            .cursor_pointer()
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(icon_box(if downloaded { file_icon(&name) } else { IconName::Download }, 40.))
                    .child(v_flex().min_w_0().child(div().truncate().font_weight(FontWeight::MEDIUM).child(name)).child(div().text_xs().text_color(t.muted_foreground).child(subtitle))),
            )
            .children(preview)
            .on_click(open)
            .into_any_element()
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !matches!(self.modal, Some(Modal::Settings)) {
            return None;
        }
        let t = cx.theme().clone();
        let sess = self.sess.as_ref()?;
        let s = self.settings.clone();
        let section = |title: &'static str| div().pt_4().pb_1().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(t.muted_foreground).child(title);
        let row = |title: &'static str, desc: &'static str| {
            v_flex().flex_1().min_w_0().child(div().text_sm().child(title)).child(div().text_xs().text_color(t.muted_foreground).child(desc))
        };
        let theme_btn = |id: &'static str, label: &'static str, value: &'static str| {
            Button::new(id)
                .label(label)
                .small()
                .when(s.theme == value, |b| b.primary())
                .when(s.theme != value, |b| b.outline())
                .on_click(cx.listener(move |this, _, w, cx| this.update_settings(w, cx, |st| st.theme = value.into())))
        };
        let me = sess.localpart();
        let body = v_flex()
            .child(section("АККАУНТ"))
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(avatar(sess, &sess.me, &me, 56.))
                    .child(v_flex().flex_1().min_w_0().child(div().font_weight(FontWeight::SEMIBOLD).child(me)).child(div().text_xs().text_color(t.muted_foreground).truncate().child(sess.me.clone())))
                    .child(Button::new("set-avatar").small().outline().label("Сменить аватар").on_click(cx.listener(|this, _, _, cx| this.change_avatar(cx))))
                    .child(Button::new("set-logout").small().danger().icon(IconName::LogOut).label("Выйти").on_click(cx.listener(|this, _, _, cx| this.logout(cx)))),
            )
            .child(section("ОФОРМЛЕНИЕ"))
            .child(h_flex().gap_2().child(theme_btn("theme-system", "Как в системе", "system")).child(theme_btn("theme-light", "Светлая", "light")).child(theme_btn("theme-dark", "Тёмная", "dark")))
            .child(section("МАСШТАБ"))
            .child(
                h_flex().gap_1().flex_wrap().children(App::SCALES.iter().enumerate().map(|(i, &scale)| {
                    let active = (s.ui_scale - scale).abs() < 0.01;
                    Button::new(SharedString::from(format!("scale-{i}")))
                        .small()
                        .label(format!("{:.0}%", scale * 100.))
                        .when(active, |b| b.primary())
                        .when(!active, |b| b.outline())
                        .on_click(cx.listener(move |this, _, w, cx| this.update_settings(w, cx, |st| st.ui_scale = scale)))
                })),
            )
            .child(div().pt_1().text_xs().text_color(t.muted_foreground).child("Также Ctrl + «+» / «−», Ctrl + 0 — сбросить"))
            .child(section("СИСТЕМА"))
            .child(
                h_flex().gap_3().py_1().items_center().child(row("Запускать при входе в систему", "Crabber будет открываться автоматически после входа в Windows или Linux")).child(
                    Switch::new("set-autostart").checked(s.autostart).on_click(cx.listener(|this, v: &bool, w, cx| {
                        crate::desktop::set_autostart(*v);
                        this.update_settings(w, cx, |st| st.autostart = crate::desktop::autostart())
                    })),
                ),
            )
            .child(section("УВЕДОМЛЕНИЯ"))
            .child(
                h_flex().gap_3().py_1().items_center().child(row("Отключить уведомления", "Не показывать всплывающие уведомления о новых сообщениях")).child(
                    Switch::new("set-notify").checked(!s.notify).on_click(cx.listener(|this, v: &bool, w, cx| {
                        let v = !*v;
                        this.update_settings(w, cx, |st| st.notify = v)
                    })),
                ),
            )
            .child(
                h_flex().gap_3().py_1().items_center().child(row("Скрывать текст в уведомлениях", "В уведомлении будет только «Новое сообщение»")).child(
                    Switch::new("set-notify-text").checked(!s.notify_text).on_click(cx.listener(|this, v: &bool, w, cx| {
                        let v = !*v;
                        this.update_settings(w, cx, |st| st.notify_text = v)
                    })),
                ),
            )
            .child(section("ШИФРОВАНИЕ (OMEMO)"))
            .child(
                h_flex().gap_3().py_1().items_center().child(row("Слепое доверие (BTBV)", "Новые устройства контактов доверенные, пока вы не подтвердили ключ контакта вручную")).child(
                    Switch::new("set-btbv").checked(s.blind_trust).on_click(cx.listener(|this, v: &bool, w, cx| {
                        let v = *v;
                        this.update_settings(w, cx, |st| st.blind_trust = v);
                        this.pgp_cmd(crabber::xmpp::Cmd::E2eBlindTrust(v));
                    })),
                ),
            )
            .children(self.device_rows(&sess.me, true, cx))
            .child(section("OPENPGP (XEP-0027)"))
            .child(match &sess.pgp_own {
                Some((fp, public)) => {
                    let public = public.clone();
                    v_flex()
                        .gap_2()
                        .child(h_flex().gap_3().items_center().child(Icon::new(IconName::Key).text_color(t.success)).child(div().text_xs().font_family("Consolas").child(fp.clone())))
                        .child(
                            h_flex()
                                .gap_2()
                                .child(Button::new("pgp-copy").small().outline().icon(IconName::Copy).label("Скопировать открытый ключ").on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(public.clone()))))
                                .child(Button::new("pgp-del").small().ghost().label("Удалить ключ").on_click(cx.listener(|this, _, _, _| this.pgp_cmd(crabber::xmpp::Cmd::PgpDeleteSecret)))),
                        )
                        .into_any_element()
                }
                None => v_flex()
                    .gap_2()
                    .child(div().text_xs().text_color(t.muted_foreground).child("Ключа нет. PGP нужен только для собеседников без OMEMO (например, Psi или старый Gajim)."))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Button::new("pgp-gen").small().primary().icon(IconName::Key).label("Создать ключ").on_click(cx.listener(|this, _, _, _| this.pgp_cmd(crabber::xmpp::Cmd::PgpGenerate))))
                            .child(Button::new("pgp-imp").small().outline().label("Импортировать закрытый ключ").on_click(cx.listener(|this, _, w, cx| this.open_pgp_import(None, w, cx)))),
                    )
                    .into_any_element(),
            })
            .child(section("ФАЙЛЫ"))
            .child(
                h_flex().gap_3().py_1().items_center().child(row("Показывать превью фото и текстовых файлов", "Скачивать их автоматически и показывать прямо в чате. Если выключено, файл скачивается только по нажатию")).child(
                    Switch::new("set-autodl").checked(s.auto_download).on_click(cx.listener(|this, v: &bool, w, cx| {
                        let v = *v;
                        this.update_settings(w, cx, |st| st.auto_download = v)
                    })),
                ),
            )
            .child(
                h_flex()
                    .gap_3()
                    .py_1()
                    .items_center()
                    .child(row("Папка загрузок", "Загрузки\\CrabberDesktop"))
                    .child(Button::new("open-dl").small().outline().icon(IconName::FolderOpen).label("Открыть").on_click(|_, _, cx| cx.open_with_system(&crabber::store::downloads_dir()))),
            )
            ;
        Some(self.panel("settings", "Настройки", body.into_any_element(), cx))
    }

    /// OMEMO devices of `jid` with fingerprints and trust switches.
    fn device_rows(&self, jid: &str, own: bool, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = cx.theme().clone();
        let Some(sess) = self.sess.as_ref() else { return vec![] };
        let Some(devices) = sess.e2e_devices.get(jid) else {
            return vec![div().py_2().text_sm().text_color(t.muted_foreground).child("Загрузка ключей...").into_any_element()];
        };
        if devices.is_empty() {
            let text = if own { "Ключи ещё не опубликованы" } else { "У собеседника нет устройств с OMEMO — сообщения уйдут без шифрования" };
            return vec![div().py_2().text_sm().text_color(t.muted_foreground).child(text).into_any_element()];
        }
        let mut list: Vec<&DeviceInfo> = devices.iter().collect();
        list.sort_by_key(|d| !d.own);
        list.into_iter()
            .map(|d| {
                let title = if d.own { format!("Это устройство · {}", d.id) } else { format!("Устройство {}", d.id) };
                let (j, id) = (jid.to_string(), d.id);
                h_flex()
                    .gap_3()
                    .py_2()
                    .items_center()
                    .child(Icon::new(if d.trusted { IconName::ShieldCheck } else { IconName::Shield }).text_color(if d.trusted { t.success } else { t.danger }))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().child(title))
                            .child(div().text_xs().font_family("Consolas").text_color(t.muted_foreground).child(d.fingerprint.clone().unwrap_or_else(|| "ключ ещё не получен".into()))),
                    )
                    .when(!d.own, |row| {
                        row.child(
                            Switch::new(SharedString::from(format!("trust-{}-{}", jid, d.id)))
                                .checked(d.trusted)
                                .tooltip("Доверять устройству")
                                .on_click(cx.listener(move |this, v: &bool, _, _| this.set_trust(j.clone(), id, *v))),
                        )
                    })
                    .into_any_element()
            })
            .collect()
    }

    fn render_encryption(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let Some(Modal::Encryption { peer }) = self.modal.as_ref() else { return None };
        let peer = peer.clone();
        let t = cx.theme().clone();
        let sess = self.sess.as_ref()?;
        let mode = sess.chat(&peer).map(|c| c.enc.clone()).unwrap_or_default();
        let mode = if mode.is_empty() { "auto".to_string() } else { mode };
        let option = |id: &'static str, value: &'static str, title: &'static str, desc: &'static str| {
            let p = peer.clone();
            let active = mode == value;
            let pal = style::palette(cx);
            h_flex()
                .id(id)
                .gap_3()
                .p_3()
                .rounded_xl()
                .border_1()
                .cursor_pointer()
                .when(active, |d| d.bg(pal.accent_soft).border_color(pal.accent))
                .when(!active, |d| d.border_color(pal.divider).hover(|d| d.border_color(pal.accent.opacity(0.5))))
                .child(
                    div()
                        .size(dp(18.))
                        .flex_none()
                        .rounded_full()
                        .border_2()
                        .border_color(if active { pal.accent } else { pal.muted })
                        .flex()
                        .items_center()
                        .justify_center()
                        .when(active, |d| d.child(div().size(dp(8.)).rounded_full().bg(pal.accent))),
                )
                .child(v_flex().flex_1().child(div().text_sm().font_weight(FontWeight::MEDIUM).child(title)).child(div().text_xs().text_color(t.muted_foreground).child(desc)))
                .on_click(cx.listener(move |this, _, _, cx| this.set_chat_enc(&p, value, cx)))
        };
        let body = v_flex()
            .gap_2()
            .child(option("enc-auto", "auto", "Автоматически (рекомендуется)", "OMEMO, если собеседник его поддерживает, иначе без шифрования"))
            .child(option("enc-omemo", "omemo", "Только OMEMO", "Не отправлять сообщение, если зашифровать невозможно"))
            .child(option("enc-pgp", "pgp", "OpenPGP (XEP-0027)", "Для клиентов без OMEMO. Шифруется только текст, файлы отправить нельзя"))
            .child(option("enc-plain", "plain", "Без шифрования", "Сообщения видны серверу"))
            .child(div().pt_4().pb_1().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(t.muted_foreground).child("УСТРОЙСТВА СОБЕСЕДНИКА"))
            .children(self.device_rows(&peer, false, cx))
            .child(div().pt_2().text_xs().text_color(t.muted_foreground).child("Сверьте отпечатки с собеседником лично или по другому каналу. Отключите доверие, если отпечаток не совпадает."))
            .child(div().pt_4().pb_1().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(t.muted_foreground).child("КЛЮЧ OPENPGP СОБЕСЕДНИКА"))
            .child({
                let (fp, hint) = sess.pgp_contacts.get(&peer).cloned().unwrap_or_default();
                let p = peer.clone();
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(Icon::new(IconName::Key).text_color(if fp.is_some() { t.success } else { t.muted_foreground }))
                    .child(div().flex_1().text_xs().font_family("Consolas").child(match (fp, hint) {
                        (Some(fp), _) => fp,
                        (None, Some(id)) => format!("Ключ {id} указан в статусе, но не найден — импортируйте его"),
                        (None, None) => "Ключ не добавлен".to_string(),
                    }))
                    .child(Button::new("pgp-imp-contact").small().outline().label("Импортировать").on_click(cx.listener(move |this, _, w, cx| this.open_pgp_import(Some(p.clone()), w, cx))))
            });
        Some(self.panel("e2e-panel", "Шифрование", body.into_any_element(), cx))
    }

    /// Right-click menu of a message, at the cursor (flipped to stay inside the window).
    fn render_msg_menu(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (m, pos) = self.msg_menu.clone()?;
        let p = style::palette(cx);
        let sess = self.sess.as_ref()?;
        let file = m.url.as_ref().and_then(|u| sess.store.file_for(u)).filter(|f| f.is_file());
        let item = |id: &'static str, icon: IconName, label: &'static str, danger: bool| {
            h_flex()
                .id(id)
                .gap_3()
                .px_3()
                .py_1p5()
                .rounded_md()
                .items_center()
                .cursor_pointer()
                .text_sm()
                .text_color(if danger { p.danger } else { p.text })
                .hover(|d| d.bg(p.hover))
                .child(Icon::new(icon).small())
                .child(label)
        };
        let mut items: Vec<AnyElement> = vec![];
        if !is_call(&m) {
            items.push(
                h_flex()
                    .justify_between()
                    .children(["👍", "❤️", "😂", "😮", "😢", "🙏", "🤝"].into_iter().map(|e| {
                        let id = m.msg_id.clone();
                        div()
                            .id(SharedString::from(format!("mm-re-{e}")))
                            .px_1()
                            .rounded_md()
                            .cursor_pointer()
                            .text_lg()
                            .hover(|d| d.bg(p.hover))
                            .child(e)
                            .on_click(cx.listener(move |this, _, _, cx| this.toggle_reaction(&id, e, cx)))
                    }))
                    .child({
                        let id = m.msg_id.clone();
                        div()
                            .id("mm-re-more")
                            .p_1()
                            .rounded_md()
                            .cursor_pointer()
                            .text_color(p.muted)
                            .hover(|d| d.bg(p.hover))
                            .child(Icon::new(IconName::Plus).small())
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.msg_menu = None;
                                this.emoji_picker = Some((id.clone(), pos, emojis::Group::SmileysAndEmotion));
                                cx.notify();
                            }))
                    })
                    .items_center()
                    .into_any_element(),
            );
            items.push(div().h(px(1.)).my_1().bg(p.divider).into_any_element());
        }
        if m.outgoing && m.url.is_none() && !is_call(&m) && m.status >= 0 {
            let msg = m.clone();
            items.push(
                item("mm-edit", IconName::Pencil, "Изменить", false)
                    .on_click(cx.listener(move |this, _, w, cx| this.start_edit(&msg, w, cx)))
                    .into_any_element(),
            );
        }
        if m.url.is_none() {
            let text = m.body.clone();
            items.push(
                item("mm-copy", IconName::Copy, "Копировать текст", false)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
                        this.msg_menu = None;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        if let Some(url) = m.url.clone() {
            let open = url.clone();
            items.push(
                item("mm-open", IconName::Image, "Открыть", false)
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.msg_menu = None;
                        this.open_attachment(open.clone(), w, cx);
                    }))
                    .into_any_element(),
            );
            items.push(
                item("mm-save", IconName::Download, "Сохранить как…", false)
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.msg_menu = None;
                        this.save_attachment_as(url.clone(), w, cx);
                    }))
                    .into_any_element(),
            );
        }
        if let Some(f) = file {
            items.push(
                item("mm-folder", IconName::FolderOpen, "Показать в папке", false)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.reveal_path(&f);
                        this.msg_menu = None;
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        let id = m.msg_id.clone();
        items.push(div().h(px(1.)).my_1().bg(p.divider).into_any_element());
        items.push(
            item("mm-delete", IconName::X, "Удалить у себя", true)
                .on_click(cx.listener(move |this, _, _, cx| this.delete_message(&id, cx)))
                .into_any_element(),
        );

        let rem = window.rem_size();
        let (w, h) = (rem * 17., rem * (2.3 * items.len() as f32 + 0.5));
        let view = window.viewport_size();
        let x = if pos.x + w > view.width { pos.x - w } else { pos.x };
        let y = if pos.y + h > view.height { (pos.y - h).max(px(0.)) } else { pos.y };
        Some(
            div()
                .id("msg-menu-backdrop")
                .absolute()
                .inset_0()
                .occlude()
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                    this.msg_menu = None;
                    cx.notify();
                }))
                .on_mouse_down(MouseButton::Right, cx.listener(|this, _, _, cx| {
                    this.msg_menu = None;
                    cx.notify();
                }))
                .child(
                    v_flex()
                        .id("msg-menu")
                        .absolute()
                        .left(x)
                        .top(y)
                        .w(w)
                        .p_1()
                        .rounded_lg()
                        .bg(p.panel)
                        .border_1()
                        .border_color(p.divider)
                        .shadow_xl()
                        // Clicks on items must not reach the backdrop's "close" handler first.
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .children(items),
                )
                .into_any_element(),
        )
    }

    /// Every emoji, one group per tab; a click toggles it as our reaction.
    fn render_emoji_picker(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (msg_id, pos, group) = self.emoji_picker.clone()?;
        let p = style::palette(cx);
        let rem = window.rem_size();
        let (w, h) = (rem * 22., rem * 20.);
        let view = window.viewport_size();
        let x = if pos.x + w > view.width { (pos.x - w).max(px(0.)) } else { pos.x };
        let y = if pos.y + h > view.height { (pos.y - h).max(px(0.)) } else { pos.y };
        let tabs = h_flex().gap_0p5().pb_1().border_b_1().border_color(p.divider).children(emojis::Group::iter().map(|g| {
            let icon = g.emojis().next().map(|e| e.as_str()).unwrap_or("?");
            div()
                .id(SharedString::from(format!("ep-tab-{g:?}")))
                .flex_1()
                .flex()
                .justify_center()
                .py_0p5()
                .rounded_md()
                .cursor_pointer()
                .when(g == group, |d| d.bg(p.hover))
                .hover(|d| d.bg(p.hover))
                .child(icon)
                .on_click(cx.listener(move |this, _, _, cx| {
                    if let Some(picker) = this.emoji_picker.as_mut() {
                        picker.2 = g;
                    }
                    cx.notify();
                }))
        }));
        let grid = div().id("ep-grid").flex_1().min_h_0().overflow_y_scroll().child(h_flex().flex_wrap().children(group.emojis().filter(|e| crate::desktop::emoji_supported(e)).map(|e| {
            let (id, emoji) = (msg_id.clone(), e.as_str());
            div()
                .id(SharedString::from(format!("ep-{emoji}")))
                .size(dp(32.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_md()
                .text_lg()
                .cursor_pointer()
                .hover(|d| d.bg(p.hover))
                .child(emoji)
                .on_click(cx.listener(move |this, _, _, cx| this.toggle_reaction(&id, emoji, cx)))
        })));
        Some(
            div()
                .id("emoji-picker-backdrop")
                .absolute()
                .inset_0()
                .occlude()
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                    this.emoji_picker = None;
                    cx.notify();
                }))
                .child(
                    v_flex()
                        .id("emoji-picker")
                        .absolute()
                        .left(x)
                        .top(y)
                        .w(w)
                        .h(h)
                        .p_1()
                        .gap_1()
                        .rounded_lg()
                        .bg(p.panel)
                        .border_1()
                        .border_color(p.divider)
                        .shadow_xl()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(tabs)
                        .child(grid),
                )
                .into_any_element(),
        )
    }

    /// Contact or group card: big avatar, address, status, and the destructive action.
    fn render_profile(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let Some(Modal::Profile { peer }) = self.modal.as_ref() else { return None };
        let peer = peer.clone();
        let p = style::palette(cx);
        let sess = self.sess.as_ref()?;
        let chat = sess.chat(&peer).cloned().unwrap_or_default();
        let name = sess.display_name(&peer);
        let status = if chat.is_room {
            match sess.occupants.get(&peer) {
                Some(n) => format!("группа · {n} в сети"),
                None => "группа".into(),
            }
        } else if sess.presence.contains(&peer) {
            "в сети".into()
        } else {
            "не в сети".into()
        };
        let has_photo = sess.avatars.contains_key(&peer);
        let (pa, pc, pd) = (peer.clone(), peer.clone(), peer.clone());
        let body = v_flex()
            .items_center()
            .gap_2()
            .pb_2()
            .child(
                div()
                    .id("profile-avatar")
                    .when(has_photo, |d| d.cursor_pointer())
                    .child(avatar(sess, &peer, &name, 120.))
                    .on_click(cx.listener(move |this, _, w, cx| this.open_avatar(&pa, w, cx))),
            )
            .child(div().pt_2().text_xl().font_weight(FontWeight::SEMIBOLD).child(name))
            .child(div().text_sm().text_color(if status == "в сети" { p.accent } else { p.muted }).child(status))
            .child(
                h_flex()
                    .id("profile-jid")
                    .gap_2()
                    .px_3()
                    .py_1()
                    .rounded_full()
                    .bg(p.field)
                    .items_center()
                    .cursor_pointer()
                    .text_sm()
                    .child(peer.clone())
                    .child(Icon::new(IconName::Copy).xsmall().text_color(p.muted))
                    .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(pc.clone()))),
            )
            .child(
                div().pt_4().w_full().child(
                    // Solid red: the theme's danger button is only a faint tint in dark mode.
                    h_flex()
                        .id("profile-delete")
                        .w_full()
                        .h(dp(40.))
                        .gap_2()
                        .rounded_lg()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .bg(p.danger)
                        .hover(|d| d.opacity(0.9))
                        .text_color(white())
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(Icon::new(if chat.is_room { IconName::DoorOpen } else { IconName::UserMinus }))
                        .child(if chat.is_room { "Покинуть группу" } else { "Удалить контакт и переписку" })
                        .on_click(cx.listener(move |this, _, w, cx| this.open_modal(Modal::ConfirmDelete { peer: pd.clone() }, w, cx))),
                ),
            );
        Some(self.panel("profile", if chat.is_room { "Группа" } else { "Контакт" }, body.into_any_element(), cx))
    }

    fn render_pgp_import(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let Some(Modal::PgpImport { jid }) = self.modal.as_ref() else { return None };
        let title = if jid.is_some() { "Открытый ключ собеседника" } else { "Импорт закрытого ключа" };
        let hint = if jid.is_some() {
            "Вставьте ключ, начинающийся с -----BEGIN PGP PUBLIC KEY BLOCK-----"
        } else {
            "Вставьте ключ без пароля, начинающийся с -----BEGIN PGP PRIVATE KEY BLOCK-----"
        };
        let t = cx.theme().clone();
        let body = v_flex()
            .gap_3()
            .child(div().text_xs().text_color(t.muted_foreground).child(hint))
            .child(Textarea::new(&self.field3))
            .child(h_flex().justify_end().child(Button::new("pgp-import-ok").primary().label("Импортировать").on_click(cx.listener(|this, _, _, cx| this.submit_pgp_import(cx)))));
        Some(self.panel("pgp-import", title, body.into_any_element(), cx))
    }

    /// Centered card over a dimmed, click-to-close backdrop.
    fn panel(&self, id: &'static str, title: &str, body: AnyElement, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme().clone();
        div()
            .id(id)
            .occlude()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(hsla(0., 0., 0., 0.5))
            .on_click(cx.listener(|this, _, _, cx| this.close_modal(cx)))
            .child(
                v_flex()
                    .id("panel-card")
                    .w(dp(560.))
                    .max_h(relative(0.85))
                    .rounded(dp(18.))
                    .bg(t.background)
                    .shadow_2xl()
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(
                        h_flex()
                            .px_5()
                            .pt_4()
                            .pb_2()
                            .items_center()
                            .child(div().flex_1().text_lg().font_weight(FontWeight::SEMIBOLD).child(title.to_string()))
                            .child(Button::new("panel-close").ghost().small().icon(IconName::X).on_click(cx.listener(|this, _, _, cx| this.close_modal(cx)))),
                    )
                    .child(div().id("panel-body").px_5().pb_5().overflow_y_scroll().child(body)),
            )
            .into_any_element()
    }

    fn render_media(&mut self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let Some(Modal::Media { peer, tab, items, links }) = self.modal.as_ref() else { return None };
        let (peer, tab, items, links) = (peer.clone(), *tab, items.clone(), links.clone());
        let t = cx.theme().clone();
        let tab_btn = |id: &'static str, label: String, value: MediaTab| {
            Button::new(id)
                .label(label)
                .small()
                .when(tab == value, |b| b.primary())
                .when(tab != value, |b| b.ghost())
                .on_click(cx.listener(move |this, _, _, cx| this.set_media_tab(value, cx)))
        };
        let photos: Vec<&Msg> = items.iter().filter(|m| m.url.as_deref().is_some_and(is_image)).collect();
        let files: Vec<&Msg> = items.iter().filter(|m| !m.url.as_deref().is_some_and(is_image)).collect();
        let urls: Vec<(String, i64)> = links
            .iter()
            .flat_map(|m| m.body.split_whitespace().filter(|w| w.starts_with("http://") || w.starts_with("https://")).map(move |w| (w.trim_end_matches([',', '.', ')', '!']).to_string(), m.ts)))
            .collect();
        let tabs = h_flex()
            .gap_1()
            .pb_3()
            .child(tab_btn("tab-photos", format!("Фото · {}", photos.len()), MediaTab::Photos))
            .child(tab_btn("tab-files", format!("Файлы · {}", files.len()), MediaTab::Files))
            .child(tab_btn("tab-links", format!("Ссылки · {}", urls.len()), MediaTab::Links));
        let empty = |text: &'static str| div().py_8().text_center().text_sm().text_color(t.muted_foreground).child(text).into_any_element();
        let content = match tab {
            MediaTab::Photos if photos.is_empty() => empty("В этом чате пока нет фото"),
            MediaTab::Photos => {
                let mut tiles = vec![];
                for m in photos {
                    let url = m.url.clone().unwrap_or_default();
                    let path = self.sess.as_ref().and_then(|s| s.store.file_for(&url)).unwrap_or_default();
                    // The gallery is opened on purpose, so fetch thumbnails regardless of the auto-download setting.
                    if !path.is_file() {
                        self.request_download(&url);
                    }
                    let u = url.clone();
                    tiles.push(
                        div()
                            .id(SharedString::from(format!("tile-{}", m.msg_id)))
                            .size(dp(122.))
                            .rounded_md()
                            .bg(t.secondary)
                            .cursor_pointer()
                            .when(path.is_file(), |d| d.child(img(path.clone()).size_full().rounded_md().object_fit(ObjectFit::Cover)))
                            .on_click(cx.listener(move |this, _, w, cx| this.open_attachment(u.clone(), w, cx))),
                    );
                }
                h_flex().flex_wrap().gap_2().children(tiles).into_any_element()
            }
            MediaTab::Files if files.is_empty() => empty("В этом чате пока нет файлов"),
            MediaTab::Files => v_flex()
                .gap_1()
                .children(files.into_iter().map(|m| {
                    let url = m.url.clone().unwrap_or_default();
                    let name = file_name(&url);
                    let path = self.sess.as_ref().and_then(|s| s.store.file_for(&url)).filter(|p| p.is_file());
                    let size = path.as_ref().and_then(|p| std::fs::metadata(p).ok()).map(|md| human_size(md.len())).unwrap_or_else(|| "не загружен".into());
                    let u = url.clone();
                    h_flex()
                        .id(SharedString::from(format!("frow-{}", m.msg_id)))
                        .gap_3()
                        .p_2()
                        .rounded_md()
                        .items_center()
                        .cursor_pointer()
                        .hover(|d| d.bg(t.accent))
                        .child(icon_box(file_icon(&name), 40.))
                        .child(v_flex().flex_1().min_w_0().child(div().truncate().text_sm().child(name)).child(div().text_xs().text_color(t.muted_foreground).child(format!("{size} · {}", short_time(m.ts)))))
                        .on_click(cx.listener(move |this, _, w, cx| this.open_attachment(u.clone(), w, cx)))
                }))
                .into_any_element(),
            MediaTab::Links if urls.is_empty() => empty("В этом чате пока нет ссылок"),
            MediaTab::Links => v_flex()
                .gap_1()
                .children(urls.into_iter().enumerate().map(|(i, (url, ts))| {
                    let u = url.clone();
                    h_flex()
                        .id(SharedString::from(format!("link-{i}")))
                        .gap_3()
                        .p_2()
                        .rounded_md()
                        .items_center()
                        .cursor_pointer()
                        .hover(|d| d.bg(t.accent))
                        .child(icon_box(IconName::Link, 40.))
                        .child(v_flex().flex_1().min_w_0().child(div().truncate().text_sm().text_color(t.link).child(url)).child(div().text_xs().text_color(t.muted_foreground).child(short_time(ts))))
                        .on_click(move |_, _, cx| cx.open_url(&u))
                }))
                .into_any_element(),
        };
        let title = format!("Медиа · {}", self.sess.as_ref().map(|s| s.display_name(&peer)).unwrap_or_default());
        Some(self.panel("media", &title, v_flex().child(tabs).child(content).into_any_element(), cx))
    }

    fn render_viewer(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let viewer = self.viewer.as_ref()?;
        let t = cx.theme().clone();
        // Picture size for the click-outside check; None = the content fills its whole box.
        let mut dims = None;
        let (title, path, body) = match viewer {
            Viewer::Image { path, url, gallery } => {
                dims = self.img_dims.get(path).copied().or_else(|| image::image_dimensions(path).ok().map(|(w, h)| (w as f32, h as f32)));
                let pos = gallery.iter().position(|u| u == url);
                let body = div()
                    .size_full()
                    .relative()
                    .child(if path.is_file() {
                        img(path.clone()).absolute().inset_0().size_full().object_fit(ObjectFit::Contain).into_any_element()
                    } else {
                        div().size_full().flex().items_center().justify_center().text_color(white()).child("Загрузка...").into_any_element()
                    })
                    .when(pos.is_some_and(|i| i > 0), |d| {
                        d.child(overlay_icon("viewer-prev", IconName::ChevronLeft).absolute().left_0().top(relative(0.5)).on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.viewer_step(-1, cx)
                        })))
                    })
                    .when(pos.is_some_and(|i| i + 1 < gallery.len()), |d| {
                        d.child(overlay_icon("viewer-next", IconName::ChevronRight).absolute().right_0().top(relative(0.5)).on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.viewer_step(1, cx)
                        })))
                    });
                let title = match pos {
                    Some(i) if gallery.len() > 1 => format!("{} · {} из {}", file_name(url), i + 1, gallery.len()),
                    _ => file_name(url),
                };
                (title, path.clone(), body.into_any_element())
            }
            Viewer::Text { name, path, text } => {
                let shown: String = text.chars().take(200_000).collect();
                let body = div()
                    .id("viewer-text")
                    .size_full()
                    .overflow_scroll()
                    .p_4()
                    .rounded_md()
                    .bg(t.background)
                    .text_color(t.foreground)
                    .font_family("Consolas")
                    .text_sm()
                    .child(shown.replace('\t', "    "))
                    .into_any_element();
                (name.clone(), path.clone(), body)
            }
        };
        // Where the picture (or text) is drawn, captured at prepaint; a click anywhere else closes the viewer.
        let content = Rc::new(Cell::new(Bounds::default()));
        let measure = {
            let content = content.clone();
            canvas(
                move |b: Bounds<Pixels>, _, _| {
                    content.set(match dims {
                        // object_fit(Contain): scaled to fit and centred.
                        Some((w, h)) if w > 0. && h > 0. => {
                            let k = (b.size.width / px(w)).min(b.size.height / px(h));
                            let sz = size(px(w * k), px(h * k));
                            Bounds::new(b.center() - point(sz.width / 2., sz.height / 2.), sz)
                        }
                        _ => b,
                    })
                },
                |_, _, _, _| {},
            )
            .absolute()
            .inset_0()
        };
        Some(
            v_flex()
                .id("viewer")
                // Block hover/clicks from reaching the chat header buttons underneath.
                .occlude()
                .absolute()
                .inset_0()
                .bg(hsla(0., 0., 0., 0.88))
                .track_focus(&self.viewer_focus)
                .on_click(cx.listener(move |this, ev: &ClickEvent, _, cx| {
                    if !content.get().contains(&ev.position()) {
                        this.viewer = None;
                        cx.notify();
                    }
                }))
                .on_key_down(cx.listener(|this, ev: &KeyDownEvent, _, cx| {
                    match ev.keystroke.key.as_str() {
                        "escape" => {
                            this.viewer = None;
                            cx.notify();
                        }
                        "left" => this.viewer_step(-1, cx),
                        "right" => this.viewer_step(1, cx),
                        _ => {}
                    }
                }))
                .child(
                    h_flex()
                        .p_3()
                        .gap_2()
                        .items_center()
                        .text_color(white())
                        .child(div().flex_1().truncate().font_weight(FontWeight::SEMIBOLD).child(title))
                        .child(overlay_icon("viewer-folder", IconName::FolderOpen).on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            cx.reveal_path(&path)
                        }))
                        .child(overlay_icon("viewer-close", IconName::X).on_click(cx.listener(|this, _, _, cx| {
                            this.viewer = None;
                            cx.notify();
                        }))),
                )
                // Absolute child of a sized box: otherwise the image grows to its own height and overflows.
                .child(div().flex_1().min_h_0().px_6().pb_6().child(div().relative().size_full().child(body).child(measure)))
                .into_any_element(),
        )
    }

    fn render_modal(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let modal = self.modal.as_ref().filter(|m| !matches!(m, Modal::Settings | Modal::Media { .. } | Modal::Encryption { .. } | Modal::PgpImport { .. } | Modal::Profile { .. }))?;
        let t = cx.theme().clone();
        let sess = self.sess.as_ref()?;
        let (title, body): (&str, AnyElement) = match modal {
            Modal::AddContact => ("Новый контакт", v_flex().gap_2().child(Input::new(&self.field1)).child(Input::new(&self.field2)).into_any_element()),
            Modal::NewGroup => ("Новая группа", Input::new(&self.field2).into_any_element()),
            Modal::JoinGroup => ("Войти в группу", Input::new(&self.field2).into_any_element()),
            Modal::Invite { room } => {
                let contacts = sess.chats.iter().filter(|c| !c.is_room && c.in_roster).map(|c| {
                    let (room, jid) = (room.clone(), c.peer.clone());
                    let name = sess.display_name(&c.peer);
                    h_flex()
                        .id(SharedString::from(format!("inv-{}", c.peer)))
                        .gap_2()
                        .p_2()
                        .rounded_md()
                        .items_center()
                        .cursor_pointer()
                        .hover(|d| d.bg(t.accent))
                        .child(avatar(sess, &c.peer, &name, 28.))
                        .child(name)
                        .on_click(cx.listener(move |this, _, w, cx| this.invite(room.clone(), jid.clone(), w, cx)))
                });
                ("Пригласить в группу", v_flex().id("invite-list").max_h(dp(320.)).overflow_y_scroll().children(contacts).into_any_element())
            }
            Modal::ConfirmDelete { peer } => {
                let room = sess.chat(peer).is_some_and(|c| c.is_room);
                let text = if room { "Покинуть группу и удалить переписку?" } else { "Удалить контакт и всю переписку?" };
                ("Подтверждение", div().child(text).into_any_element())
            }
            Modal::Settings | Modal::Media { .. } | Modal::Encryption { .. } | Modal::PgpImport { .. } | Modal::Profile { .. } => return None,
        };
        let confirm = match modal {
            Modal::Invite { .. } => None,
            Modal::ConfirmDelete { peer } => {
                let p = peer.clone();
                Some(Button::new("modal-ok").danger().label("Удалить").on_click(cx.listener(move |this, _, _, cx| this.delete_chat(p.clone(), cx))))
            }
            _ => Some(Button::new("modal-ok").primary().label("Готово").on_click(cx.listener(|this, _, w, cx| this.submit_modal(w, cx)))),
        };
        Some(
            div()
                .id("modal-overlay")
                .occlude()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(hsla(0., 0., 0., 0.5))
                .on_click(cx.listener(|this, _, _, cx| this.close_modal(cx)))
                .child(
                    v_flex()
                        .id("modal")
                        .w(dp(420.))
                        .p_5()
                        .gap_4()
                        .rounded_lg()
                        .bg(t.background)
                        .border_1()
                        .border_color(t.border)
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(title))
                        .child(body)
                        .child(
                            h_flex()
                                .gap_2()
                                .justify_end()
                                .child(Button::new("modal-cancel").ghost().label("Отмена").on_click(cx.listener(|this, _, _, cx| this.close_modal(cx))))
                                .children(confirm),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_call(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sess = self.sess.as_ref()?;
        let call = sess.call.as_ref()?;
        let p = style::palette(cx);
        let name = sess.display_name(&call.peer);
        let status = match &call.state {
            CallState::Outgoing => "Вызов...".to_string(),
            CallState::Incoming => "Входящий аудиозвонок".to_string(),
            CallState::Connecting => "Соединение...".to_string(),
            CallState::Active => {
                let secs = call.started.map_or(0, |s| s.elapsed().as_secs());
                format!("{:02}:{:02}", secs / 60, secs % 60)
            }
            CallState::Ended(r) => r.clone(),
        };
        let incoming = call.state == CallState::Incoming;
        let muted = call.muted;
        let round = |id: &'static str, icon: IconName, bg: Hsla, fg: Hsla| {
            div().id(id).size(dp(52.)).rounded_full().bg(bg).cursor_pointer().flex().items_center().justify_center().child(Icon::new(icon).size(dp(22.)).text_color(fg))
        };
        let card_bg = linear_gradient(170., linear_color_stop(rgb(0x2B5278), 0.), linear_color_stop(rgb(0x17212B), 1.));
        let card = v_flex()
            .occlude()
            .w(dp(300.))
            .p_5()
            .gap_3()
            .items_center()
            .rounded(dp(20.))
            .shadow_2xl()
            .bg(card_bg)
            .text_color(white())
            .child(avatar(sess, &call.peer, &name, 84.))
            .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(name))
            .child(div().text_sm().opacity(0.75).child(status))
            .child(
                h_flex()
                    .pt_2()
                    .gap_6()
                    .when(incoming, |d| d.child(round("call-accept", IconName::Phone, p.online, white()).on_click(cx.listener(|this, _, _, cx| this.call(CallCmd::Accept, cx)))))
                    .when(!incoming, |d| {
                        d.child(
                            round("call-mute", if muted { IconName::MicOff } else { IconName::Mic }, if muted { white() } else { hsla(0., 0., 1., 0.15) }, if muted { rgb(0x17212B).into() } else { white() })
                                .on_click(cx.listener(move |this, _, _, cx| this.call(CallCmd::Mute(!muted), cx))),
                        )
                    })
                    .child(round("call-hangup", IconName::PhoneOff, p.danger, white()).on_click(cx.listener(|this, _, _, cx| this.call(CallCmd::Hangup, cx)))),
            );
        // Only the card occludes: the rest of the window stays clickable during a call.
        Some(div().absolute().inset_0().flex().items_center().justify_center().child(card).into_any_element())
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: that would pull in gpui's own `test` macro.
    use super::{Seg, initials, segments};

    #[test]
    fn code_fences_and_initials() {
        assert_eq!(
            segments("look:\n```rust\nfn main() {}\n```\ndone"),
            [Seg::Text("look:"), Seg::Code { lang: "rust", code: "fn main() {}" }, Seg::Text("done")]
        );
        assert_eq!(segments("```a b```"), [Seg::Code { lang: "", code: "a b" }]);
        assert_eq!(segments("broken ``` fence"), [Seg::Text("broken "), Seg::Text("```"), Seg::Text(" fence")]);
        assert_eq!(initials("bitcoin_casino"), "BC");
        assert_eq!(initials("Bob"), "B");
        assert_eq!(initials("иван петров"), "ИП");
    }

    #[test]
    fn urls_in_text() {
        let t = "см. https://a.io/x?y=1, и (http://b.ru). не ссылка: http:// ok";
        let found: Vec<&str> = super::url_ranges(t).into_iter().map(|r| &t[r]).collect();
        assert_eq!(found, ["https://a.io/x?y=1", "http://b.ru"]);
    }

    #[test]
    fn link_click_opens_url() {
        use gpui_kit::*;
        struct V;
        impl Render for V {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().w(px(400.)).child(super::rich_text("m".into(), "https://example.com tail", black()))
            }
        }
        let mut cx = TestAppContext::single();
        cx.update(gpui_kit::init);
        let window = cx.add_window(|window, cx| {
            let view = cx.new(|_| V);
            base::Root::new(view, window, cx)
        });
        let cx = &mut VisualTestContext::from_window(window.into(), &cx);
        cx.update(|window, cx| drop(window.draw(cx)));
        // Click on the plain word after the link: nothing opens.
        cx.simulate_click(point(px(390.), px(8.)), Modifiers::default());
        assert_eq!(cx.opened_url(), None);
        cx.simulate_click(point(px(10.), px(8.)), Modifiers::default());
        assert_eq!(cx.opened_url().as_deref(), Some("https://example.com"));
    }

    #[test]
    fn message_text_is_selectable_and_copied_while_composer_focused() {
        use gpui_kit::*;
        use gpui_kit::component::input::{Copy as CopyAction, Textarea, TextareaState};
        struct V(ListState, Entity<TextareaState>);
        impl Render for V {
            fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                let rows = |ix: usize, _: &mut Window, _: &mut gpui_kit::App| super::Selectable::new(("t", ix), format!("alpha beta {ix}"), vec![]).into_any_element();
                div()
                    .size_full()
                    .child(div().w(px(240.)).h(px(200.)).child(list(self.0.clone(), rows).size_full()))
                    .child(div().capture_action(cx.listener(|this, _: &CopyAction, window, cx| {
                        if super::copy_message_selection(&this.1, window, cx) {
                            cx.stop_propagation();
                        }
                    })).child(Textarea::new(&self.1)))
            }
        }
        let mut cx = TestAppContext::single();
        cx.update(gpui_kit::init);
        let window = cx.add_window(|window, cx| {
            let compose = cx.new(|cx| TextareaState::new(window, cx));
            compose.read(cx).focus_handle(cx).focus(window, cx);
            let view = cx.new(|_| V(ListState::new(3, ListAlignment::Top, px(100.)), compose));
            base::Root::new(view, window, cx)
        });
        let cx = &mut VisualTestContext::from_window(window.into(), &cx);
        cx.update(|window, cx| drop(window.draw(cx)));
        cx.simulate_mouse_down(point(px(1.), px(8.)), MouseButton::Left, Modifiers::default());
        cx.simulate_mouse_move(point(px(230.), px(8.)), Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(point(px(230.), px(8.)), MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| drop(window.draw(cx)));
        cx.simulate_keystrokes("ctrl-c");
        assert_eq!(cx.read_from_clipboard().and_then(|c| c.text()).as_deref(), Some("alpha beta 0"));
    }
}

#[cfg(test)]
mod frame_bench {
    use gpui_kit::prelude::FluentBuilder as _;
    use gpui_kit::{AppContext as _, Context, IntoElement, ListAlignment, ListState, ParentElement as _, Render, Styled as _, TestAppContext, Window, black, div, list, px};
    struct V(ListState, bool);
    impl Render for V {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let rows = |ix: usize, _: &mut Window, _: &mut gpui_kit::App| div().p_2().child(super::Selectable::new(("t", ix), format!("message number {ix} with some words in it"), vec![])).into_any_element();
            div().size(px(1100.)).relative().when(self.1, |d| d.child(super::doodles(black()))).child(list(self.0.clone(), rows).size_full())
        }
    }
    #[test]
    fn scroll_to_bottom_button_in_long_chat() {
        let mut cx = TestAppContext::single();
        cx.update(gpui_kit::init);
        let list = ListState::new(500, ListAlignment::Top, px(40.));
        list.set_follow_mode(gpui_kit::FollowMode::Tail);
        let l = list.clone();
        let (_, cx) = cx.add_window_view(move |_, _| V(l, false));
        cx.update(|w, cx| drop(w.draw(cx)));
        assert_eq!(list.is_scrolled_to_end(), None, "gpui cannot tell in a long chat");
        assert!(!super::below_bottom(&list), "at the end");
        list.scroll_by(px(-300.));
        cx.update(|w, cx| drop(w.draw(cx)));
        assert!(super::below_bottom(&list), "scrolled up");
        list.set_follow_mode(gpui_kit::FollowMode::Tail);
        cx.update(|w, cx| drop(w.draw(cx)));
        assert!(!super::below_bottom(&list), "back at the end");
    }

    #[test]
    #[ignore]
    fn frame_time() {
        for doodles in [false, true] {
            let mut cx = TestAppContext::single();
            cx.update(gpui_kit::init);
            let (_, cx) = cx.add_window_view(|_, _| V(ListState::new(500, ListAlignment::Top, px(40.)), doodles));
            cx.update(|w, cx| drop(w.draw(cx)));
            let t = std::time::Instant::now();
            for _ in 0..100 {
                cx.update(|w, cx| {
                    w.refresh();
                    drop(w.draw(cx));
                });
            }
            eprintln!("doodles={doodles}: {:.2} ms/frame", t.elapsed().as_secs_f64() * 10.);
        }
    }
}
