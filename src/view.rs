//! Rendering. Telegram-like layout: chat list on the left, conversation on the right.
use crabber::t;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use chrono::{Datelike, Local, TimeZone};
use gpui_kit::assets::IconName;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, h_flex,
    button::{Button, ButtonCustomVariant, ButtonVariants as _},
    checkbox::Checkbox,
    color_picker::ColorSelect,
    menu::{DropdownMenu as _, PopupMenu, PopupMenuItem},
    resizable::{resizable_panel, v_resizable},
    slider::{Slider, SliderState},
    switch::Switch,
    tooltip::Tooltip,
    input::{Input, Paste, Textarea, TextareaState},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use crabber::call::{CallCmd, CallState};
use crabber::groupcall::GroupCmd;
use crabber::audio::MediaDevice;
use crabber::videocall::Source;
use crabber::store::{ChatRow, Msg};
use tokio_xmpp::parsers::presence::Show;

use crate::app::{App, DeviceKind, MediaTab, Modal, Session, Viewer, ext, file_name, is_audio, is_call, is_image, is_text, is_video, my_occupant};
use crate::style;
use crabber::e2e::DeviceInfo;

/// Design pixels in rems (16 px = 1 rem), so `Window::set_rem_size` scales the whole UI.
// Material Icons (Apache-2.0) for the message bar.
const ATTACH_SVG: &[u8] = include_bytes!("../assets/icons/attach_file.svg");
const MOOD_SVG: &[u8] = include_bytes!("../assets/icons/mood.svg");
const MIC_SVG: &[u8] = include_bytes!("../assets/icons/mic.svg");
const SEND_SVG: &[u8] = include_bytes!("../assets/icons/send.svg");

fn dp(x: f32) -> Rems {
    rems(x / 16.)
}

/// The scheme editor's editable colors, in `App::color_pickers` order.
fn color_fields() -> [&'static str; 6] {
    [t!("colors.accent"), t!("colors.panel"), t!("colors.chat"), t!("colors.bubble_in"), t!("colors.bubble_out"), t!("colors.text")]
}

/// Month name in the date form `date.day_month` expects.
fn month_name(m0: u32) -> &'static str {
    t!("date.months").split(',').nth(m0 as usize).unwrap_or_default()
}

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
        t!("date.today").into()
    } else if Some(t.date_naive()) == today.pred_opt() {
        t!("date.yesterday").into()
    } else if t.year() == Local::now().year() {
        t!("date.day_month", day = t.day(), month = month_name(t.month0()))
    } else {
        t!("date.day_month_year", day = t.day(), month = month_name(t.month0()), year = t.year())
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

/// A run of message text: consecutive `> quote` lines, or plain text between them.
enum QuotePart {
    Text(String),
    Quote(String),
}

/// Splits text into plain and quote runs; quote markers (`>` and one space) are stripped.
fn split_quotes(text: &str) -> Vec<QuotePart> {
    let mut parts: Vec<(bool, String)> = vec![];
    for line in text.split('\n') {
        let trimmed = line.trim_start();
        let (quote, stripped) = match trimmed.strip_prefix('>') {
            Some(rest) => (true, rest.strip_prefix(' ').unwrap_or(rest)),
            None => (false, line),
        };
        match parts.last_mut() {
            Some((last, buf)) if *last == quote => {
                buf.push('\n');
                buf.push_str(stripped);
            }
            _ => parts.push((quote, stripped.to_string())),
        }
    }
    parts.into_iter().map(|(q, text)| if q { QuotePart::Quote(text) } else { QuotePart::Text(text) }).collect()
}

/// Maps a ``` fence tag or file extension to a tree-sitter language id.
fn lang_id(tag: &str) -> &str {
    match tag.to_ascii_lowercase().as_str() {
        "py" | "python3" => "python",
        "js" | "mjs" | "cjs" => "javascript",
        "ts" => "typescript",
        "sh" | "shell" | "zsh" => "bash",
        "c++" | "cc" | "cxx" | "hpp" => "cpp",
        "cs" => "csharp",
        "golang" => "go",
        "htm" => "html",
        "kt" => "kotlin",
        "md" => "markdown",
        "rb" => "ruby",
        "rs" => "rust",
        "yml" => "yaml",
        _ => tag,
    }
}

/// Syntax-highlight styles for `code`, cached by (mode, language, text): blocks re-render every frame.
fn syntax_styles(dark: bool, lang: &str, code: &str) -> Arc<Vec<(std::ops::Range<usize>, HighlightStyle)>> {
    use gpui_kit::component::{Rope, highlighter::{HighlightTheme, SyntaxHighlighter}};
    static CACHE: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<(bool, String, String), Arc<Vec<(std::ops::Range<usize>, HighlightStyle)>>>>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));
    let lang = lang_id(lang);
    let key = (dark, lang.to_string(), code.to_string());
    let mut cache = CACHE.lock().unwrap();
    if let Some(styles) = cache.get(&key) {
        return styles.clone();
    }
    let mut hl = SyntaxHighlighter::new(lang);
    // A huge paste must not stall a frame: a timeout leaves it unstyled.
    hl.update(None, &Rope::from_str(code), Some(std::time::Duration::from_millis(100)));
    let theme = if dark { HighlightTheme::default_dark() } else { HighlightTheme::default_light() };
    let styles = Arc::new(hl.styles(&(0..code.len()), &*theme));
    if cache.len() > 100 {
        cache.clear();
    }
    cache.insert(key, styles.clone());
    styles
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
/// Device choice: the list is read when the menu opens, so newly plugged devices show up.
fn device_items(menu: PopupMenu, kind: DeviceKind, current: Option<MediaDevice>, this: WeakEntity<App>) -> PopupMenu {
    let item = |name: SharedString, d: Option<MediaDevice>| {
        let this = this.clone();
        let checked = d.as_ref().map(|d| &d.id) == current.as_ref().map(|d| &d.id);
        PopupMenuItem::new(name).checked(checked).on_click(move |_, w, cx| {
            let d = d.clone();
            let _ = this.update(cx, |app, cx| app.update_settings(w, cx, |st| *kind.slot(st) = d));
        })
    };
    let mut menu = menu.max_h(px(360.)).scrollable(true).item(item(t!("settings.device_default").into(), None));
    for d in kind.list() {
        menu = menu.item(item(d.name.clone().into(), Some(d)));
    }
    menu
}

/// A menu row with a checkbox on the right, visible also when off.
fn check_item(label: &'static str, on: bool) -> PopupMenuItem {
    PopupMenuItem::element(move |_, cx| {
        let t = cx.theme();
        let (accent, border) = (t.primary, t.border);
        h_flex().w_full().gap_4().justify_between().items_center().child(label).child(
            div()
                .size(px(16.))
                .flex_none()
                .rounded(px(4.))
                .border_1()
                .flex()
                .items_center()
                .justify_center()
                .map(|d| if on { d.bg(accent).border_color(accent).child(Icon::new(IconName::Check).size(px(12.)).text_color(white())) } else { d.border_color(border) }),
        )
    })
}

/// A volume slider row inside a menu; disabled so clicking it doesn't close the menu.
fn volume_item(label: &'static str, state: Entity<SliderState>) -> PopupMenuItem {
    PopupMenuItem::element(move |_, cx| {
        let v = state.read(cx).value().start();
        v_flex().w(px(240.)).gap_1().py_1().child(h_flex().justify_between().text_sm().child(label).child(format!("{v:.0}%"))).child(Slider::new(&state))
    })
    .disabled(true)
}

/// The arrow next to a call button: its menu, opening upwards; arrow up while open.
fn menu_chevron(
    id: &'static str,
    this: WeakEntity<App>,
    open: bool,
    (fg, soft): (Hsla, Hsla),
    cx: &gpui_kit::App,
    builder: impl Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu + 'static,
) -> impl IntoElement {
    Button::new(id)
        .custom(ButtonCustomVariant::new(cx).foreground(fg).hover(soft).active(soft))
        .xsmall()
        .rounded_full()
        .icon(if open { IconName::ChevronUp } else { IconName::ChevronDown })
        .dropdown_menu_with_anchor(Anchor::BottomLeft, builder)
        .on_open_change(move |open, _, cx| {
            let _ = this.update(cx, |app, cx| {
                app.call_menu = open.then_some(id);
                cx.notify();
            });
        })
}

fn quality_label(q: crabber::videocall::Quality) -> &'static str {
    use crabber::videocall::Quality;
    match q {
        Quality::Source => t!("call.quality_source"),
        Quality::P1440 => t!("call.quality_1440"),
        Quality::P1080 => t!("call.quality_1080"),
        Quality::P720 => t!("call.quality_720"),
    }
}

fn call_status(call: &crate::app::CallUi) -> String {
    match &call.state {
        CallState::Outgoing if call.peer_ringing => t!("call.ringing").to_string(),
        CallState::Outgoing => t!("call.calling").to_string(),
        CallState::Incoming => t!("call.incoming").to_string(),
        CallState::Connecting => t!("call.connecting").to_string(),
        CallState::Active => {
            let secs = call.started.map_or(0, |s| s.elapsed().as_secs());
            let time = format!("{:02}:{:02}", secs / 60, secs % 60);
            if call.peer_held { format!("{} · {time}", t!("call.on_hold")) } else { time }
        }
        CallState::Ended(r) => r.clone(),
    }
}

/// Nicks of the occupants of `room` taking part in its group call.
fn room_callers(sess: &Session, room: &str) -> Vec<String> {
    let mut nicks: Vec<String> = sess.members.get(room).into_iter().flat_map(|m| m.values()).filter(|o| o.call).map(|o| o.nick.clone()).collect();
    nicks.sort();
    nicks
}

/// "Waiting for others…", or how many are in the call and for how long.
fn group_status(g: &crate::app::GroupCallUi) -> String {
    if g.peers.is_empty() {
        return t!("gcall.waiting").to_string();
    }
    let secs = g.started.elapsed().as_secs();
    format!("{} · {:02}:{:02}", t!("gcall.people", n = g.peers.len() + 1), secs / 60, secs % 60)
}

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
        n if n >= 1 << 20 => t!("unit.mb", n = format!("{:.1}", n as f64 / (1 << 20) as f64)),
        n if n >= 1 << 10 => t!("unit.kb", n = format!("{:.0}", n as f64 / (1 << 10) as f64)),
        n => t!("unit.b", n = n),
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
    let text = if m.retracted {
        t!("msg.deleted").to_string()
    } else if m.url.is_some() {
        if is_image(&m.body) { t!("media.photo").to_string() } else if is_video(&m.body) { t!("media.video").to_string() } else if is_audio(&m.body) { t!("media.voice").to_string() } else { t!("media.file").to_string() }
    } else {
        m.body.replace("```", " ").split_whitespace().collect::<Vec<_>>().join(" ")
    };
    let prefix = if is_call(m) { String::new() } else if m.outgoing { format!("{}: ", t!("msg.you")) } else if c.is_room { format!("{}: ", m.sender) } else { String::new() };
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
            .when_some(self.render_viewer(cx), |d, v| d.child(v))
            .when_some(self.render_update(cx), |d, u| d.child(u));
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
            .when(cfg!(windows) && !self.call_fullscreen, |d| d.child(self.render_titlebar(window, cx)))
            .child(content)
            .when_some(self.render_msg_menu(window, cx), |d, m| d.child(m))
            .when_some(self.render_emoji_picker(window, cx), |d, m| d.child(m))
            .when_some(self.render_viewer_menu(cx), |d, m| d.child(m))
            // Full screen hides the title bar; the mouse at the top edge slides it down over the
            // call until it leaves the bar.
            .when(cfg!(windows) && self.call_fullscreen, |d| {
                let peek = self.titlebar_peek;
                d.child(
                    div()
                        .id("titlebar-peek")
                        .absolute()
                        .top_0()
                        .left_0()
                        .right_0()
                        .when(!peek, |d| d.h(dp(6.)))
                        .when(peek, |d| d.shadow_lg().child(self.render_titlebar(window, cx)))
                        .occlude()
                        .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                            if this.titlebar_peek != *hovered {
                                this.titlebar_peek = *hovered;
                                cx.notify();
                            }
                        })),
                )
            })
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
        let fullscreen = self.call_fullscreen;
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
                .when(!fullscreen, |d| d.window_control_area(area))
                .child(glyph_el)
        };
        let maximized = window.is_maximized() || fullscreen;
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
                    // A full-screen window isn't dragged around.
                    .when(!fullscreen, |d| d.window_control_area(WindowControlArea::Drag))
                    .child(img(self.logo.clone()).size(dp(20.)))
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("Crabber"))
                    .when(unread > 0, |d| {
                        d.child(div().px_1p5().h(dp(18.)).rounded_full().flex().items_center().bg(p.accent).text_color(white()).text_xs().font_weight(FontWeight::SEMIBOLD).child(unread.to_string()))
                    })
                    .when(offline, |d| d.child(div().text_xs().text_color(p.muted).child(t!("titlebar.connecting"))))
                    .when(self.settings.ui_scale != 1.0, |d| d.child(div().text_xs().text_color(p.muted).child(format!("· {:.0}%", self.settings.ui_scale * 100.)))),
            )
            // Full screen turns the system hit-test areas off, so there the buttons act themselves;
            // the middle one leaves full screen.
            .child(
                control("win-min", line(10., 1.).into_any_element(), WindowControlArea::Min, false)
                    .when(fullscreen, |d| d.on_click(|_, w, _| w.minimize_window())),
            )
            .child(control("win-max", max_glyph, WindowControlArea::Max, false).when(fullscreen, |d| {
                d.tooltip(|w, cx| Tooltip::new(t!("call.exit_fullscreen")).build(w, cx))
                    .on_click(cx.listener(|this, _, w, cx| this.set_call_fullscreen(false, w, cx)))
            }))
            .child(
                control("win-close", Icon::new(IconName::X).size(dp(15.)).into_any_element(), WindowControlArea::Close, true)
                    .when(fullscreen, |d| d.on_click(cx.listener(|this, _, w, cx| this.close_window(w, cx)))),
            )
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
                        .child(div().text_sm().text_color(p.muted).child(t!("login.subtitle"))),
                )
                .child(Input::new(&self.login_jid))
                .child(Input::new(&self.login_pass).mask_toggle())
                .child(Input::new(&self.login_host))
                .child(
                    Checkbox::new("insecure")
                        .label(t!("login.insecure"))
                        .checked(self.insecure)
                        .on_click(cx.listener(|this, v: &bool, _, cx| {
                            this.insecure = *v;
                            cx.notify();
                        })),
                )
                .child(
                    Checkbox::new("remember")
                        .label(t!("login.remember"))
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
                        .label(t!("login.submit"))
                        .loading(self.connecting)
                        .on_click(cx.listener(|this, _, _, cx| this.login(cx))),
                )
                .when(!self.login_error.is_empty(), |d| {
                    d.child(div().p_2().rounded_md().bg(p.danger.opacity(0.1)).text_sm().text_color(p.danger).child(self.login_error.clone()))
                }),
        )
    }

    fn render_main(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.restore_call_view(window, cx);
        if self.call_expanded {
            match self.render_call_panel(cx) {
                Some(panel) => return panel,
                // The call ended while expanded.
                None => {
                    (self.call_expanded, self.call_fullscreen) = (false, false);
                    if window.is_fullscreen() {
                        window.toggle_fullscreen();
                    }
                }
            }
        }
        let strip = self.render_call_strip(cx).or_else(|| self.render_group_call_strip(cx));
        let conversation = self.render_conversation(window, cx).into_any_element();
        h_flex()
            .size_full()
            .child(self.render_sidebar(cx))
            .child(v_flex().flex_1().h_full().min_w_0().children(strip).child(h_flex().flex_1().min_h_0().child(conversation)))
            .into_any_element()
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let p = style::palette(cx);
        let sess = self.sess.as_ref().unwrap();
        let query = self.search.read(cx).value().to_lowercase();
        let (show_label, show_color) = match sess.show {
            None => (t!("status.online"), p.online),
            Some(Show::Away) => (t!("status.away"), rgb(0xF5A623).into()),
            Some(Show::Dnd) => (t!("status.dnd"), p.danger),
            _ => (t!("status.online"), p.online),
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
                            .child(if sess.online { show_label } else { t!("status.no_connection") })
                            .on_click(cx.listener(|this, _, _, cx| this.cycle_show(cx))),
                    ),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .child(Button::new("add-contact").ghost().small().icon(IconName::UserPlus).tooltip(t!("sidebar.add_contact")).on_click(cx.listener(|this, _, w, cx| this.open_modal(Modal::AddContact, w, cx))))
                    .child(Button::new("new-group").ghost().small().icon(IconName::Users).tooltip(t!("group.new")).on_click(cx.listener(|this, _, w, cx| this.open_modal(Modal::NewGroup, w, cx))))
                    .child(Button::new("join-group").ghost().small().icon(IconName::Hash).tooltip(t!("sidebar.join_group")).on_click(cx.listener(|this, _, w, cx| this.open_modal(Modal::JoinGroup, w, cx))))
                    .child(Button::new("settings").ghost().small().icon(IconName::Settings).tooltip(t!("settings.title")).on_click(cx.listener(|this, _, _, cx| this.open_settings(cx)))),
            );

        let search = div().mx_3().mb_2().px_3().py_0p5().rounded_full().bg(p.field).child(
            Input::new(&self.search).appearance(false).cleanable(true).prefix(Icon::new(IconName::Search).text_color(p.muted)),
        );

        let requests = sess.requests.iter().map(|jid| {
            let (a, d, sp) = (jid.clone(), jid.clone(), jid.clone());
            let can_block = sess.blocked.is_some();
            v_flex()
                .mx_2()
                .mb_2()
                .p_3()
                .gap_2()
                .rounded_xl()
                .bg(p.accent_soft)
                .child(div().text_sm().child(t!("request.text", jid = jid)))
                .child(
                    h_flex()
                        .gap_2()
                        .child(Button::new(SharedString::from(format!("acc-{jid}"))).primary().small().label(t!("request.accept")).on_click(cx.listener(move |this, _, _, cx| this.answer_request(a.clone(), true, cx))))
                        .child(Button::new(SharedString::from(format!("rej-{jid}"))).ghost().small().label(t!("request.decline")).on_click(cx.listener(move |this, _, _, cx| this.answer_request(d.clone(), false, cx))))
                        .when(can_block, |d| {
                            d.child(Button::new(SharedString::from(format!("spam-{jid}"))).ghost().small().label(t!("profile.spam")).tooltip(t!("request.spam_tip")).on_click(cx.listener(move |this, _, _, cx| {
                                this.answer_request(sp.clone(), false, cx);
                                this.block(sp.clone(), true, true, cx);
                            })))
                        }),
                )
        });

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
                let sub = if typing { t!("typing").to_string() } else { preview(c) };
                let locked = c.last.as_ref().is_some_and(|m| m.enc > 0);
                // The row is filled with the accent: its text must contrast with it (not always white).
                let on_acc = style::on_color(p.accent);
                let (title_c, sub_c, time_c) = if active { (on_acc, on_acc.opacity(0.75), on_acc.opacity(0.75)) } else { (p.text, if typing { p.accent } else { p.muted }, p.muted) };
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
                                                .bg(if active { on_acc } else { p.accent })
                                                .text_color(if active { p.accent } else { on_acc })
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
                        .label(t!("search.in_chat", chat = chat_name))
                        .when(!all, |b| b.primary())
                        .when(all, |b| b.ghost())
                        .on_click(cx.listener(|this, _, _, cx| this.set_search_all(false, cx))),
                )
                .child(
                    Button::new("scope-all")
                        .small()
                        .label(t!("search.all_chats"))
                        .when(all, |b| b.primary())
                        .when(!all, |b| b.ghost())
                        .on_click(cx.listener(|this, _, _, cx| this.set_search_all(true, cx))),
                )
        });

        let hits = (!self.search_results.is_empty()).then(|| {
            v_flex()
                .child(div().px_4().pt_3().pb_1().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(p.muted).child(if scoped { t!("search.this_chat_header") } else { t!("search.messages_header") }))
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
                        .child(div().text_sm().text_color(p.muted).child(t!("chats.empty"))),
                )
            })
            .when_some(self.update.as_ref().filter(|_| !self.update_popup && self.update_hidden_until.is_none_or(|t| std::time::Instant::now() >= t)), |d, rel| {
                d.child(
                    h_flex()
                        .id("update-reminder")
                        .m_2()
                        .p_3()
                        .gap_3()
                        .items_center()
                        .rounded_xl()
                        .bg(p.accent)
                        .hover(|d| d.bg(p.accent_hover))
                        .text_color(white())
                        .cursor_pointer()
                        .child(Icon::new(IconName::Download))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(t!("update.available")))
                                .child(div().text_xs().opacity(0.85).truncate().child(if self.updating { t!("update.downloading_banner", percent = self.update_percent()) } else { t!("update.banner_version", version = rel.version) })),
                        )
                        .child(
                            div()
                                .id("update-reminder-close")
                                .p_1()
                                .rounded_md()
                                .hover(|d| d.bg(hsla(0., 0., 1., 0.15)))
                                .child(Icon::new(IconName::X).small())
                                .on_click(cx.listener(|this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.hide_update_reminder(cx)
                                })),
                        )
                        .on_click(cx.listener(|this, _, _, cx| this.show_update(true, cx))),
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
                .child(div().px_4().py_1p5().rounded_full().bg(p.chip).text_sm().text_color(p.chip_text).child(t!("chat.select")))
                .when(!sess.online, |d| d.child(div().px_3().py_1().rounded_full().bg(p.chip).text_xs().text_color(p.chip_text).child(sess.status.clone())))
                .into_any_element();
        };
        let chat = sess.chat(&peer).cloned().unwrap_or_default();
        let name = sess.display_name(&peer);
        let typing = sess.typing.get(&peer).filter(|(_, at)| at.elapsed().as_secs() < 30).map(|(who, _)| who.clone());
        let online = sess.presence.contains(&peer);
        let (subtitle, highlight) = if let Some(who) = typing {
            (if chat.is_room { t!("typing.who", who = who) } else { t!("typing").into() }, true)
        } else if chat.is_room {
            (match sess.occupants.get(&peer) {
                Some(n) => t!("room.online", n = n),
                None => t!("room.label").into(),
            }, false)
        } else if online {
            (t!("status.online").into(), true)
        } else if chat.in_roster && (chat.subscription == "none" || chat.subscription == "from") {
            (t!("status.pending").into(), false)
        } else {
            (t!("status.offline").into(), false)
        };

        // Group call in this room: who is in it, and whether we can join from here.
        let in_group_call = sess.group_call.as_ref().is_some_and(|g| g.room == peer);
        let callers = room_callers(sess, &peer);
        let can_group_call = sess.online && sess.call.is_none() && sess.group_call.is_none() && sess.occupants.contains_key(&peer);
        let call_banner = (chat.is_room && !in_group_call && !callers.is_empty()).then(|| {
            let room = peer.clone();
            h_flex()
                .flex_shrink_0()
                .px_4()
                .py_2()
                .gap_3()
                .items_center()
                .bg(p.online.opacity(0.12))
                .border_b_1()
                .border_color(p.border)
                .child(Icon::new(IconName::Phone).size(dp(16.)).text_color(p.online))
                .child(div().flex_1().min_w_0().truncate().text_sm().child(t!("gcall.ongoing", who = callers.join(", "))))
                .child(
                    Button::new("group-call-join")
                        .primary()
                        .small()
                        .label(t!("gcall.join"))
                        .disabled(!can_group_call)
                        .on_click(cx.listener(move |this, _, _, cx| this.group_call(GroupCmd::Join { room: room.clone() }, cx))),
                )
        });

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
                        .tooltip(t!("chat.call"))
                        .disabled(sess.call.is_some() || sess.group_call.is_some() || !sess.online)
                        .on_click(cx.listener(move |this, _, _, cx| this.call(CallCmd::Start { to: to.clone(), video: false }, cx))),
                )
            })
            .when(!chat.is_room, |d| {
                let to = peer.clone();
                d.child(
                    Button::new("video-call")
                        .ghost()
                        .icon(IconName::Video)
                        .tooltip(t!("chat.video_call"))
                        .disabled(sess.call.is_some() || sess.group_call.is_some() || !sess.online)
                        .on_click(cx.listener(move |this, _, _, cx| this.call(CallCmd::Start { to: to.clone(), video: true }, cx))),
                )
            })
            .when(!chat.is_room, |d| {
                let pr = peer.clone();
                let secure = matches!(chat.enc.as_str(), "" | "auto" | "omemo") && sess.e2e_devices.get(&peer).is_none_or(|d| !d.is_empty());
                d.child(
                    Button::new("e2e")
                        .ghost()
                        .icon(if secure || chat.enc == "pgp" { IconName::Lock } else { IconName::LockOpen })
                        .tooltip(t!("e2e.title"))
                        .on_click(cx.listener(move |this, _, _, cx| this.open_encryption(pr.clone(), cx))),
                )
            })
            .when(chat.is_room && !in_group_call, |d| {
                let room = peer.clone();
                d.child(
                    Button::new("group-call")
                        .ghost()
                        .icon(IconName::Phone)
                        .tooltip(if callers.is_empty() { t!("gcall.start") } else { t!("gcall.join") })
                        .disabled(!can_group_call)
                        .on_click(cx.listener(move |this, _, _, cx| this.group_call(GroupCmd::Join { room: room.clone() }, cx))),
                )
            })
            .child({
                let pr = peer.clone();
                Button::new("media").ghost().icon(IconName::Image).tooltip(t!("chat.media")).on_click(cx.listener(move |this, _, _, cx| this.open_media(pr.clone(), cx)))
            })
            .when(chat.is_room, |d| {
                let room = peer.clone();
                d.child(Button::new("invite").ghost().icon(IconName::UserPlus).tooltip(t!("chat.invite")).on_click(cx.listener(move |this, _, w, cx| this.open_modal(Modal::Invite { room: room.clone() }, w, cx))))
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
                let name = f.file_name().and_then(|n| n.to_str()).unwrap_or(t!("media.file_fallback")).to_string();
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
                    .child(Button::new(SharedString::from(format!("rm-{ix}"))).ghost().xsmall().icon(IconName::X).tooltip(t!("compose.remove")).on_click(cx.listener(move |this, _, _, cx| this.remove_pending(ix, cx))))
            }))
        });

        let bar = |icon: IconName, title: String, text: String| {
            h_flex()
                .flex_shrink_0()
                .px_4()
                .pt_2()
                .gap_3()
                .items_center()
                .bg(p.panel)
                .border_t_1()
                .border_color(p.border)
                .child(Icon::new(icon).small().text_color(p.accent))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(p.accent).truncate().child(title))
                        .child(div().text_sm().text_color(p.muted).truncate().child(text.replace('\n', " "))),
                )
        };
        let editing = sess.editing.as_ref().and_then(|id| sess.history.iter().find(|m| m.msg_id == *id)).map(|m| {
            bar(IconName::Pencil, t!("compose.editing").into(), m.body.clone())
                .child(Button::new("edit-cancel").ghost().xsmall().icon(IconName::X).tooltip(t!("compose.cancel")).on_click(cx.listener(|this, _, w, cx| this.cancel_edit(w, cx))))
        });
        let replying = sess.replying.as_ref().map(|m| {
            let who = if m.outgoing { t!("compose.reply_self").to_string() } else if chat.is_room { m.sender.clone() } else { sess.display_name(&peer) };
            bar(IconName::Reply, t!("compose.reply_to", who = who), m.url.clone().unwrap_or_else(|| m.body.clone()))
                .child(Button::new("reply-cancel").ghost().xsmall().icon(IconName::X).tooltip(t!("compose.cancel")).on_click(cx.listener(|this, _, _, cx| this.cancel_reply(cx))))
        });
        let editing = editing.or(replying);
        let blocked = !chat.is_room && sess.blocked.as_ref().is_some_and(|b| b.contains(&peer));
        let blocked_bar = blocked.then(|| {
            let jid = peer.clone();
            h_flex()
                .flex_shrink_0()
                .px_4()
                .py_2()
                .gap_3()
                .items_center()
                .justify_center()
                .bg(p.panel)
                .border_t_1()
                .border_color(p.border)
                .text_sm()
                .text_color(p.muted)
                .child(t!("chat.blocked"))
                .child(Button::new("unblock").small().ghost().label(t!("block.unblock")).on_click(cx.listener(move |this, _, _, cx| this.block(jid.clone(), false, false, cx))))
        });

        let has_text = !self.compose.read(cx).value().trim().is_empty() || !sess.pending.is_empty() || sess.editing.is_some();
        // Telegram-like bar: one flat strip, translucent Material icons. Buttons and the field's first line are
        // 44 px tall, so one line of text lines up with them; with more lines the buttons stay at the bottom.
        let icon_btn = |id: &'static str, svg: &'static [u8], color: Hsla| {
            div()
                .id(id)
                .size(dp(44.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .text_color(color)
                .hover(move |d| d.text_color(Hsla { a: 0.85, ..color }))
                .child(Icon::empty().data(svg).size(dp(24.)))
        };
        let dim = p.text.opacity(0.45);
        let recording = self.recording.as_ref().map(|(r, at)| (at.elapsed().as_secs(), r.level()));
        let bar = h_flex()
            .flex_shrink_0()
            .px_1()
            .items_end()
            .bg(p.panel)
            .when(pending.is_none() && editing.is_none(), |d| d.border_t_1().border_color(p.border));
        let composer = if let Some((secs, level)) = recording {
            bar.child(
                div()
                    .id("rec-cancel")
                    .size(dp(44.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .cursor_pointer()
                    .text_color(dim)
                    .hover(|d| d.text_color(p.text))
                    .child(Icon::new(IconName::X).size(dp(20.)))
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_recording(cx))),
            )
            .child(
                h_flex()
                    .flex_1()
                    .h(dp(44.))
                    .gap_2()
                    .items_center()
                    .child(div().size(dp(10.)).rounded_full().bg(p.danger))
                    .child(div().text_sm().child(t!("compose.recording", time = format!("{}:{:02}", secs / 60, secs % 60))))
                    // Level meter: sqrt so speech fills a visible part of the bar.
                    .child(div().w(dp(80.)).h(px(4.)).rounded_full().bg(hsla(0., 0., 0.5, 0.3)).child(div().h_full().rounded_full().bg(p.accent).w(relative(level.sqrt().min(1.))))),
            )
            .child(icon_btn("rec-send", SEND_SVG, p.accent).on_click(cx.listener(|this, _, w, cx| this.finish_recording(w, cx))))
            .into_any_element()
        } else {
            bar.child(icon_btn("attach", ATTACH_SVG, dim).on_click(cx.listener(|this, _, _, cx| this.attach(cx))))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .min_h(dp(44.))
                        .flex()
                        .items_center()
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
                        .child(div().w_full().child(Textarea::new(&self.compose).appearance(false))),
                )
                .child(icon_btn("emoji", MOOD_SVG, dim).on_click(cx.listener(|this, ev: &ClickEvent, _, cx| {
                    this.emoji_picker = Some((None, ev.position(), emojis::Group::SmileysAndEmotion));
                    cx.notify();
                })))
                .child(if has_text {
                    icon_btn("send", SEND_SVG, p.accent).on_click(cx.listener(|this, _, window, cx| this.send_text(window, cx))).into_any_element()
                } else {
                    icon_btn("mic", MIC_SVG, dim).on_click(cx.listener(|this, _, w, cx| this.start_recording(w, cx))).into_any_element()
                })
                .into_any_element()
        };

        let chat_area = v_flex()
            .size_full()
            .min_h_0()
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
            .children(blocked_bar)
            .when(!blocked, |d| d.child(composer));
        let panel = match in_group_call {
            true => self.render_group_call_panel(cx),
            false => sess.call.as_ref().filter(|c| c.peer == peer).and_then(|_| self.render_call_panel(cx)),
        };
        let body = match panel {
            Some(panel) if self.call_chat_hidden => panel,
            Some(panel) => v_resizable("call-split")
                .with_state(&self.call_split)
                .child(resizable_panel().size(px(360.)).size_range(px(200.)..px(4000.)).child(panel))
                .child(resizable_panel().size_range(px(160.)..px(4000.)).child(chat_area))
                .into_any_element(),
            None => chat_area.into_any_element(),
        };
        v_flex()
            .flex_1()
            .h_full()
            .min_w_0()
            .child(header)
            .children(call_banner)
            .when(!sess.online, |d| d.child(div().px_4().py_1().text_xs().bg(p.danger.opacity(0.12)).text_color(p.danger).child(sess.status.clone())))
            .child(div().flex_1().min_h_0().flex().child(body))
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

        let quote = m.reply_to.as_ref().map(|id| {
            let orig = sess.history.iter().find(|q| q.msg_id == *id).cloned().or_else(|| sess.store.msg(sess.selected.as_deref().unwrap_or_default(), id));
            let who = match &orig {
                Some(o) if o.outgoing => t!("msg.you").to_string(),
                Some(o) if is_room => o.sender.clone(),
                Some(_) => sess.selected.as_deref().map(|p| sess.display_name(p)).unwrap_or_default(),
                None => t!("msg.message").to_string(),
            };
            let text = match &orig {
                Some(o) if o.retracted => t!("msg.deleted").to_string(),
                Some(o) if o.url.is_some() => t!("media.file").to_string(),
                Some(o) => o.body.replace('\n', " "),
                None => t!("msg.unavailable").to_string(),
            };
            (id.clone(), orig.map(|o| o.ts), who, text, sess.selected.clone().unwrap_or_default())
        });
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
                    .flex()
                    .gap_1()
                    .child(div().font_family(style::EMOJI_FONT).child(e.clone()))
                    .child(n.to_string())
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_reaction(&id, &emoji, cx)))
            }))
        });
        let content = match &m.url {
            _ if m.retracted => h_flex().gap_2().items_center().italic().opacity(0.7).child(Icon::new(IconName::Ban).small()).child(t!("msg.deleted")).into_any_element(),
            _ if is_call(&m) => h_flex().gap_2().items_center().child(Icon::new(IconName::Phone).small()).child(m.body.clone()).into_any_element(),
            Some(url) => self.render_attachment(url, &m, cx),
            None => v_flex()
                .gap_1()
                .children(segments(&m.body).into_iter().enumerate().flat_map(|(i, seg)| {
                    match seg {
                        Seg::Text(text) => split_quotes(text)
                            .into_iter()
                            .enumerate()
                            .map(|(j, part)| match part {
                                QuotePart::Text(text) => rich_text(SharedString::from(format!("txt-{i}-{j}-{}", m.msg_id)).into(), &text, code).into_any_element(),
                                // A quote block, styled like the reply preview above the message.
                                QuotePart::Quote(text) => h_flex()
                                    .gap_2()
                                    .pr_2()
                                    .py_0p5()
                                    .rounded_md()
                                    .bg(code)
                                    .child(div().w(px(4.)).self_stretch().rounded_l_md().bg(if m.outgoing { fg } else { sender_color(&m.sender) }))
                                    .child(v_flex().flex_1().min_w_0().child(rich_text(SharedString::from(format!("qt-{i}-{j}-{}", m.msg_id)).into(), &text, code)))
                                    .into_any_element(),
                            })
                            .collect::<Vec<_>>(),
                        Seg::Code { lang, code: src } => vec![{
                            let copy = src.to_string();
                            let shown = src.replace('\t', "    ");
                            // A solid dark panel with light text: a tint would vanish on any bubble.
                            let panel = hsla(0., 0., 0.07, 1.);
                            let hl = if lang.is_empty() { Arc::default() } else { syntax_styles(true, lang, &shown) };
                            v_flex()
                                .min_w(dp(260.))
                                .rounded_lg()
                                .bg(panel)
                                .text_color(rgb(0xE8E8E8))
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
                                        .child(if lang.is_empty() { t!("code.label").to_string() } else { lang.to_string() })
                                        .child(
                                            // Plain icon so it takes the bubble's text colour (a Button would not on dark bubbles).
                                            div()
                                                .id(SharedString::from(format!("copy-{i}-{}", m.msg_id)))
                                                .p_1()
                                                .rounded_sm()
                                                .cursor_pointer()
                                                .hover(|d| d.bg(hsla(0., 0., 1., 0.1)))
                                                .child(Icon::new(IconName::Copy).xsmall())
                                                .on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(copy.clone()))),
                                        ),
                                )
                                .child(div().px_3().pb_2().font_family("Consolas").text_sm().child(Selectable::new(SharedString::from(format!("src-{i}-{}", m.msg_id)), shown, (*hl).clone())))
                                .into_any_element()
                        }],
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
            .when(m.edited, |d| d.child(t!("msg.edited")))
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
            .children(quote.map(|(id, ts, who, text, peer)| {
                h_flex()
                    .id(SharedString::from(format!("q-{}", m.msg_id)))
                    .gap_2()
                    .pr_2()
                    .py_0p5()
                    .rounded_md()
                    .bg(code)
                    .cursor_pointer()
                    .child(div().w(px(4.)).self_stretch().rounded_l_md().bg(if m.outgoing { fg } else { sender_color(&who) }))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).truncate().child(who))
                            .child(div().text_sm().opacity(0.8).truncate().child(text)),
                    )
                    .when_some(ts, |d, ts| d.on_click(cx.listener(move |this, _, w, cx| this.open_message(peer.clone(), id.clone(), ts, w, cx))))
            }))
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
        if is_audio(url) {
            let p = style::palette(cx);
            if !downloaded && !failed && self.settings.auto_download {
                self.request_download(url);
            }
            let progress = self.player.as_ref().filter(|(u, _)| u == url).map(|(_, pl)| pl.progress());
            let len = if downloaded {
                *self.audio_len.entry(path.clone()).or_insert_with(|| std::fs::read(&path).ok().and_then(|b| crabber::voice::duration(&b)).unwrap_or(0.))
            } else {
                0.
            };
            let clock = |secs: f32| format!("{}:{:02}", secs as u32 / 60, secs as u32 % 60);
            let label = match progress {
                Some(f) => format!("{} / {}", clock(f * len), clock(len)),
                None if downloaded => clock(len),
                None if failed => t!("media.unavailable_retry").to_string(),
                None => t!("media.voice").to_string(),
            };
            let u = url.to_string();
            return h_flex()
                .gap_3()
                .items_center()
                .min_w(dp(240.))
                .child(
                    div()
                        .id(SharedString::from(format!("play-{}", m.msg_id)))
                        .size(dp(40.))
                        .flex_shrink_0()
                        .rounded_full()
                        .bg(p.accent)
                        .hover(|d| d.bg(p.accent_hover))
                        .cursor_pointer()
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(Icon::new(if progress.is_some() { IconName::Pause } else { IconName::Play }).text_color(white()))
                        .on_click(cx.listener(move |this, _, w, cx| this.toggle_play(u.clone(), w, cx))),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .gap_1()
                        .child(div().h(px(4.)).w_full().rounded_full().bg(hsla(0., 0., 0.5, 0.3)).child(div().h_full().rounded_full().bg(p.accent).w(relative(progress.unwrap_or(0.)))))
                        .child(div().text_xs().opacity(0.7).child(label)),
                )
                .into_any_element();
        }
        let video = is_video(url) && crabber::video::available();
        if is_image(url) || video {
            let kind = if video { t!("media.video") } else { t!("media.image") };
            if !downloaded {
                if failed {
                    return div()
                        .id(SharedString::from(format!("img-{}", m.msg_id)))
                        .cursor_pointer()
                        .text_sm()
                        .opacity(0.7)
                        .child(t!("media.kind_unavailable", kind = kind))
                        .on_click(open)
                        .into_any_element();
                }
                let requested = self.sess.as_ref().is_some_and(|s| s.downloads.contains(url));
                if !self.settings.auto_download && !requested {
                    return div()
                        .id(SharedString::from(format!("img-{}", m.msg_id)))
                        .cursor_pointer()
                        .child(h_flex().gap_2().items_center().child(icon_box(IconName::Download, 40.)).child(div().text_sm().child(if video { t!("media.video_click") } else { t!("media.photo_click") })))
                        .on_click(open)
                        .into_any_element();
                }
                self.request_download(url);
                return div().text_sm().opacity(0.7).child(if video { t!("media.loading_video") } else { t!("media.loading_image") }).into_any_element();
            }
            if video {
                let thumb = self.video_thumb(&path, cx);
                let (w, h) = match &thumb {
                    Some(t) => *self.img_dims.entry(t.clone()).or_insert_with(|| image::image_dimensions(t).map(|(w, h)| (w as f32, h as f32)).unwrap_or((320., 180.))),
                    None => (320., 180.),
                };
                let scale = (320. / w).min(320. / h).min(1.);
                return div()
                    .id(SharedString::from(format!("vid-{}", m.msg_id)))
                    .cursor_pointer()
                    .relative()
                    .w(dp(w * scale))
                    .h(dp(h * scale))
                    .rounded_md()
                    .bg(black())
                    .when_some(thumb, |d, t| d.child(img(t).size_full().rounded_md()))
                    .child(
                        div()
                            .absolute()
                            .inset_0()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(div().size(dp(48.)).rounded_full().bg(hsla(0., 0., 0., 0.55)).flex().items_center().justify_center().child(Icon::new(IconName::Play).text_color(white()))),
                    )
                    .on_click(open)
                    .into_any_element();
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
            if is_text(url) { t!("file.open_hint", size = size) } else { t!("file.folder_hint", size = size) }
        } else if failed {
            t!("file.unavailable").to_string()
        } else {
            t!("file.click_download").to_string()
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
        let scheme_btn = |id: &'static str, label: &'static str, value: &'static str| {
            Button::new(id)
                .label(label)
                .small()
                .when(s.scheme == value, |b| b.primary())
                .when(s.scheme != value, |b| b.outline())
                .on_click(cx.listener(move |this, _, w, cx| this.update_settings(w, cx, |st| st.scheme = value.into())))
        };
        let lang_btn = |id: SharedString, label: &'static str, value: &'static str| {
            Button::new(id)
                .label(label)
                .small()
                .when(s.lang == value, |b| b.primary())
                .when(s.lang != value, |b| b.outline())
                .on_click(cx.listener(move |this, _, w, cx| this.update_settings(w, cx, |st| st.lang = value.into())))
        };
        let this = cx.entity().downgrade();
        let device_row = |id: &'static str, title: &'static str, kind: DeviceKind| {
            let current = kind.slot(&mut s.clone()).clone();
            let label: SharedString = current.as_ref().map_or_else(|| t!("settings.device_default").into(), |d| d.name.clone().into());
            let this = this.clone();
            h_flex().gap_3().py_1().items_center().child(div().flex_1().min_w_0().text_sm().child(title)).child(
                Button::new(id).small().outline().max_w(dp(280.)).label(label).dropdown_menu(move |menu, _, _| device_items(menu, kind, current.clone(), this.clone())),
            )
        };
        let me = sess.localpart();
        let body = v_flex()
            .child(section(t!("settings.account")))
            .child(
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(avatar(sess, &sess.me, &me, 56.))
                    .child(v_flex().flex_1().min_w_0().child(div().font_weight(FontWeight::SEMIBOLD).child(me)).child(div().text_xs().text_color(t.muted_foreground).truncate().child(sess.me.clone())))
                    .child(Button::new("set-avatar").small().outline().label(t!("settings.change_avatar")).on_click(cx.listener(|this, _, _, cx| this.change_avatar(cx))))
                    .child(Button::new("set-logout").small().danger().icon(IconName::LogOut).label(t!("settings.logout")).on_click(cx.listener(|this, _, _, cx| this.logout(cx)))),
            )
            .child(section(t!("settings.theme")))
            .child(h_flex().gap_2().flex_wrap().child(theme_btn("theme-system", t!("settings.theme_system"), "system")).child(theme_btn("theme-light", t!("settings.theme_light"), "light")).child(theme_btn("theme-dark", t!("settings.theme_dark"), "dark")))
            .child(section(t!("settings.language")))
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(lang_btn("lang-system".into(), t!("settings.lang_system"), "system"))
                    .children(crabber::i18n::Lang::ALL.map(|l| lang_btn(format!("lang-{}", l.code()).into(), l.name(), l.code()))),
            )
            .child(section(t!("settings.accent")))
            .child(
                h_flex()
                    .gap_1()
                    .flex_wrap()
                    .child(scheme_btn("scheme-default", t!("settings.scheme_default"), "default"))
                    .children(style::PRESETS.iter().map(|p| {
                        Button::new(SharedString::from(format!("scheme-{}", p.0)))
                            .label(crabber::i18n::get(p.1))
                            .small()
                            .when(s.scheme == p.0, |b| b.primary())
                            .when(s.scheme != p.0, |b| b.outline())
                            .on_click(cx.listener(move |this, _, w, cx| this.update_settings(w, cx, |st| st.scheme = p.0.into())))
                    }))
                    .child(scheme_btn("scheme-custom", t!("settings.scheme_custom"), "custom")),
            )
            .when(s.scheme == "custom", |d| {
                d.child(
                    v_flex()
                        .gap_2()
                        .pt_2()
                        .children([(0usize, t!("settings.light_scheme")), (6, t!("settings.dark_scheme"))].map(|(off, title)| {
                            v_flex()
                                .gap_1()
                                .child(div().text_xs().text_color(t.muted_foreground).child(title))
                                .child(h_flex().gap_2().flex_wrap().children(color_fields().into_iter().enumerate().map(|(j, label)| {
                                    v_flex().gap_1().w(dp(104.)).child(div().text_xs().text_color(t.muted_foreground).child(label)).child(ColorSelect::new(&self.color_pickers[off + j]))
                                })))
                        }))
                        .child(Button::new("colors-reset").small().outline().label(t!("settings.reset_colors")).on_click(cx.listener(|this, _, w, cx| this.reset_custom_colors(w, cx)))),
                )
            })
            .when(cfg!(target_os = "linux"), |d| {
                d.child(section(t!("settings.accent_from")))
                    .child(
                        h_flex().gap_1().flex_wrap()
                            .child(scheme_btn("scheme-matugen", "Matugen", "matugen").when(!style::source_available("matugen"), |b| b.disabled(true)))
                            .child(scheme_btn("scheme-dux", "Dux", "dux").when(!style::source_available("dux"), |b| b.disabled(true))),
                    )
            })
            .child(section(t!("settings.scale")))
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
            .child(div().pt_1().text_xs().text_color(t.muted_foreground).child(t!("settings.scale_hint")))
            .child(section(t!("settings.system")))
            .child(
                h_flex().gap_3().py_1().items_center().child(row(t!("settings.autostart"), t!("settings.autostart_desc"))).child(
                    Switch::new("set-autostart").checked(s.autostart).on_click(cx.listener(|this, v: &bool, w, cx| {
                        crate::desktop::set_autostart(*v, this.settings.start_minimized);
                        this.update_settings(w, cx, |st| st.autostart = crate::desktop::autostart())
                    })),
                ),
            )
            .child(
                h_flex().gap_3().py_1().items_center().child(row(t!("settings.start_min"), t!("settings.start_min_desc"))).child(
                    Switch::new("set-start-min").checked(s.start_minimized).on_click(cx.listener(|this, v: &bool, w, cx| {
                        let v = *v;
                        this.update_settings(w, cx, |st| st.start_minimized = v);
                        if this.settings.autostart {
                            crate::desktop::set_autostart(true, this.settings.start_minimized);
                            this.update_settings(w, cx, |st| st.autostart = crate::desktop::autostart());
                        }
                    })),
                ),
            )
            .child(section(t!("settings.notifications")))
            .child(
                h_flex().gap_3().py_1().items_center().child(row(t!("settings.notify_off"), t!("settings.notify_off_desc"))).child(
                    Switch::new("set-notify").checked(!s.notify).on_click(cx.listener(|this, v: &bool, w, cx| {
                        let v = !*v;
                        this.update_settings(w, cx, |st| st.notify = v)
                    })),
                ),
            )
            .child(
                h_flex().gap_3().py_1().items_center().child(row(t!("settings.hide_text"), t!("settings.hide_text_desc"))).child(
                    Switch::new("set-notify-text").checked(!s.notify_text).on_click(cx.listener(|this, v: &bool, w, cx| {
                        let v = !*v;
                        this.update_settings(w, cx, |st| st.notify_text = v)
                    })),
                ),
            )
            .child(section(t!("settings.devices")))
            .child(device_row("dev-mic", t!("settings.mic"), DeviceKind::Mic))
            .child(device_row("dev-speaker", t!("settings.speaker"), DeviceKind::Speaker))
            .child(device_row("dev-camera", t!("settings.camera"), DeviceKind::Camera))
            .child(
                h_flex().gap_3().py_1().items_center().child(row(t!("settings.denoise"), t!("settings.denoise_desc"))).child(
                    Switch::new("set-denoise").checked(s.denoise).on_click(cx.listener(|this, v: &bool, w, cx| {
                        let v = *v;
                        this.update_settings(w, cx, |st| st.denoise = v)
                    })),
                ),
            )
            .child(div().pt_1().text_xs().text_color(t.muted_foreground).child(t!("settings.devices_hint")))
            .child(section(t!("settings.omemo")))
            .child(
                h_flex().gap_3().py_1().items_center().child(row(t!("settings.btbv"), t!("settings.btbv_desc"))).child(
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
                                .child(Button::new("pgp-copy").small().outline().icon(IconName::Copy).label(t!("settings.pgp_copy")).on_click(move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(public.clone()))))
                                .child(Button::new("pgp-del").small().ghost().label(t!("settings.pgp_delete")).on_click(cx.listener(|this, _, _, _| this.pgp_cmd(crabber::xmpp::Cmd::PgpDeleteSecret)))),
                        )
                        .into_any_element()
                }
                None => v_flex()
                    .gap_2()
                    .child(div().text_xs().text_color(t.muted_foreground).child(t!("settings.pgp_none")))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(Button::new("pgp-gen").small().primary().icon(IconName::Key).label(t!("settings.pgp_generate")).on_click(cx.listener(|this, _, _, _| this.pgp_cmd(crabber::xmpp::Cmd::PgpGenerate))))
                            .child(Button::new("pgp-imp").small().outline().label(t!("settings.pgp_import")).on_click(cx.listener(|this, _, w, cx| this.open_pgp_import(None, w, cx)))),
                    )
                    .into_any_element(),
            })
            .child(section(t!("settings.files")))
            .child(
                h_flex().gap_3().py_1().items_center().child(row(t!("settings.auto_dl"), t!("settings.auto_dl_desc"))).child(
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
                    .child(row(t!("settings.downloads"), t!("settings.downloads_path")))
                    .child(Button::new("open-dl").small().outline().icon(IconName::FolderOpen).label(t!("action.open")).on_click(|_, _, cx| cx.open_with_system(&crabber::store::downloads_dir()))),
            )
            .child(div().pt_4().text_xs().text_center().text_color(t.muted_foreground).child(format!("Crabber {}", env!("CARGO_PKG_VERSION"))))
            ;
        Some(self.panel("settings", t!("settings.title"), body.into_any_element(), cx))
    }

    /// OMEMO devices of `jid` with fingerprints and trust switches.
    fn device_rows(&self, jid: &str, own: bool, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let t = cx.theme().clone();
        let Some(sess) = self.sess.as_ref() else { return vec![] };
        let Some(devices) = sess.e2e_devices.get(jid) else {
            return vec![div().py_2().text_sm().text_color(t.muted_foreground).child(t!("e2e.loading")).into_any_element()];
        };
        if devices.is_empty() {
            let text = if own { t!("e2e.own_none") } else { t!("e2e.peer_none") };
            return vec![div().py_2().text_sm().text_color(t.muted_foreground).child(text).into_any_element()];
        }
        let mut list: Vec<&DeviceInfo> = devices.iter().collect();
        list.sort_by_key(|d| !d.own);
        list.into_iter()
            .map(|d| {
                let title = if d.own { t!("e2e.this_device", id = d.id) } else { t!("e2e.device", id = d.id) };
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
                            .child(div().text_xs().font_family("Consolas").text_color(t.muted_foreground).child(d.fingerprint.clone().unwrap_or_else(|| t!("e2e.no_fingerprint").into()))),
                    )
                    .when(!d.own, |row| {
                        row.child(
                            Switch::new(SharedString::from(format!("trust-{}-{}", jid, d.id)))
                                .checked(d.trusted)
                                .tooltip(t!("e2e.trust"))
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
            .child(option("enc-auto", "auto", t!("e2e.auto"), t!("e2e.auto_desc")))
            .child(option("enc-omemo", "omemo", t!("e2e.omemo"), t!("e2e.omemo_desc")))
            .child(option("enc-pgp", "pgp", "OpenPGP (XEP-0027)", t!("e2e.pgp_desc")))
            .child(option("enc-plain", "plain", t!("e2e.plain"), t!("e2e.plain_desc")))
            .child(div().pt_4().pb_1().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(t.muted_foreground).child(t!("e2e.peer_devices")))
            .children(self.device_rows(&peer, false, cx))
            .child(div().pt_2().text_xs().text_color(t.muted_foreground).child(t!("e2e.verify_hint")))
            .child(div().pt_4().pb_1().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(t.muted_foreground).child(t!("e2e.peer_pgp")))
            .child({
                let (fp, hint) = sess.pgp_contacts.get(&peer).cloned().unwrap_or_default();
                let p = peer.clone();
                h_flex()
                    .gap_3()
                    .items_center()
                    .child(Icon::new(IconName::Key).text_color(if fp.is_some() { t.success } else { t.muted_foreground }))
                    .child(div().flex_1().text_xs().font_family("Consolas").child(match (fp, hint) {
                        (Some(fp), _) => fp,
                        (None, Some(id)) => t!("e2e.pgp_hint_missing", id = id),
                        (None, None) => t!("e2e.pgp_none").to_string(),
                    }))
                    .child(Button::new("pgp-imp-contact").small().outline().label(t!("pgp.import")).on_click(cx.listener(move |this, _, w, cx| this.open_pgp_import(Some(p.clone()), w, cx))))
            });
        Some(self.panel("e2e-panel", t!("e2e.title"), body.into_any_element(), cx))
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
        let room = sess.selected.as_deref().filter(|p| sess.chat(p).is_some_and(|c| c.is_room));
        let moderator = room.and_then(|r| my_occupant(sess, r)).is_some_and(|o| o.role == "moderator");
        let has_sid = room.is_some_and(|r| sess.store.sid(r, &m.msg_id).is_some());
        if !is_call(&m) && !m.retracted {
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
                            .font_family(style::EMOJI_FONT)
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
                                this.emoji_picker = Some((Some(id.clone()), pos, emojis::Group::SmileysAndEmotion));
                                cx.notify();
                            }))
                    })
                    .items_center()
                    .into_any_element(),
            );
            items.push(div().h(px(1.)).my_1().bg(p.divider).into_any_element());
        }
        if !is_call(&m) && !m.retracted && m.status >= 0 {
            let msg = m.clone();
            items.push(
                item("mm-reply", IconName::Reply, t!("menu.reply"), false)
                    .on_click(cx.listener(move |this, _, w, cx| this.start_reply(&msg, w, cx)))
                    .into_any_element(),
            );
        }
        if m.outgoing && m.url.is_none() && !is_call(&m) && m.status >= 0 {
            let msg = m.clone();
            items.push(
                item("mm-edit", IconName::Pencil, t!("menu.edit"), false)
                    .on_click(cx.listener(move |this, _, w, cx| this.start_edit(&msg, w, cx)))
                    .into_any_element(),
            );
        }
        if m.url.is_none() && !m.retracted {
            let text = m.body.clone();
            items.push(
                item("mm-copy", IconName::Copy, t!("menu.copy_text"), false)
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
                item("mm-open", IconName::Image, t!("action.open"), false)
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.msg_menu = None;
                        this.open_attachment(open.clone(), w, cx);
                    }))
                    .into_any_element(),
            );
            items.push(
                item("mm-save", IconName::Download, t!("menu.save_as"), false)
                    .on_click(cx.listener(move |this, _, w, cx| {
                        this.msg_menu = None;
                        this.save_attachment_as(url.clone(), w, cx);
                    }))
                    .into_any_element(),
            );
        }
        if let Some(f) = file.clone().filter(|_| m.url.as_deref().is_some_and(is_image)) {
            items.push(
                item("mm-copy-img", IconName::Copy, t!("menu.copy_image"), false)
                    .on_click(cx.listener(move |this, _, _, cx| this.copy_image(&f, cx)))
                    .into_any_element(),
            );
        }
        if let Some(f) = file {
            items.push(
                item("mm-folder", IconName::FolderOpen, t!("menu.show_in_folder"), false)
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
        let can_retract = !m.retracted && !is_call(&m) && m.status >= 0 && (m.outgoing || (moderator && has_sid));
        if can_retract {
            let (msg, moderate) = (m.clone(), !m.outgoing);
            items.push(
                item("mm-retract", IconName::Ban, if moderate { t!("menu.retract_mod") } else { t!("menu.retract") }, true)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.msg_menu = None;
                        this.modal = Some(Modal::ConfirmRetract { msg: msg.clone(), moderate });
                        cx.notify();
                    }))
                    .into_any_element(),
            );
        }
        items.push(
            item("mm-delete", IconName::X, t!("menu.delete_local"), true)
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
                .font_family(style::EMOJI_FONT)
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
                .font_family(style::EMOJI_FONT)
                .cursor_pointer()
                .hover(|d| d.bg(p.hover))
                .child(emoji)
                .on_click(cx.listener(move |this, _, w, cx| match &id {
                    Some(id) => this.toggle_reaction(id, emoji, cx),
                    None => this.insert_emoji(emoji, w, cx),
                }))
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
                Some(n) => t!("room.online_profile", n = n),
                None => t!("room.label").into(),
            }
        } else if sess.presence.contains(&peer) {
            t!("status.online").into()
        } else {
            t!("status.offline").into()
        };
        let has_photo = sess.avatars.contains_key(&peer);
        let (pa, pc, pd) = (peer.clone(), peer.clone(), peer.clone());
        let blocked = sess.blocked.as_ref().map(|b| b.contains(&peer));
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
            .child(div().text_sm().text_color(if status == t!("status.online") { p.accent } else { p.muted }).child(status))
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
            .when(chat.is_room, |d| d.child(self.render_members(&peer, cx)))
            .when_some(blocked.filter(|_| !chat.is_room), |d, blocked| {
                let (pb, ps) = (peer.clone(), peer.clone());
                d.child(
                    h_flex()
                        .pt_4()
                        .w_full()
                        .gap_2()
                        .child(
                            Button::new("profile-block")
                                .flex_1()
                                .icon(IconName::Ban)
                                .label(if blocked { t!("block.unblock") } else { t!("block.block") })
                                .on_click(cx.listener(move |this, _, _, cx| this.block(pb.clone(), !blocked, false, cx))),
                        )
                        .when(!blocked, |d| {
                            d.child(
                                Button::new("profile-spam")
                                    .flex_1()
                                    .icon(IconName::TriangleAlert)
                                    .label(t!("profile.spam"))
                                    .tooltip(t!("profile.spam_tip"))
                                    .on_click(cx.listener(move |this, _, _, cx| this.block(ps.clone(), true, true, cx))),
                            )
                        }),
                )
            })
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
                        .child(if chat.is_room { t!("profile.leave") } else { t!("profile.delete") })
                        .on_click(cx.listener(move |this, _, w, cx| this.open_modal(Modal::ConfirmDelete { peer: pd.clone() }, w, cx))),
                ),
            );
        Some(self.panel("profile", if chat.is_room { t!("profile.group_title") } else { t!("profile.contact_title") }, body.into_any_element(), cx))
    }

    /// Occupants of a group; moderators get kick, owners/admins also ban and (un)make moderator.
    fn render_members(&self, room: &str, cx: &mut Context<Self>) -> AnyElement {
        let p = style::palette(cx);
        let Some(sess) = self.sess.as_ref() else { return div().into_any_element() };
        let me = my_occupant(sess, room).cloned();
        let is_mod = me.as_ref().is_some_and(|o| o.role == "moderator");
        let is_admin = me.as_ref().is_some_and(|o| o.affiliation == "owner" || o.affiliation == "admin");
        let mut list: Vec<crabber::xmpp::Occupant> = sess.members.get(room).map(|m| m.values().cloned().collect()).unwrap_or_default();
        let rank = |o: &crabber::xmpp::Occupant| match o.affiliation.as_str() {
            "owner" => 0,
            "admin" => 1,
            _ if o.role == "moderator" => 2,
            _ => 3,
        };
        list.sort_by(|a, b| rank(a).cmp(&rank(b)).then(a.nick.to_lowercase().cmp(&b.nick.to_lowercase())));
        let rows = list.into_iter().map(|o| {
            let badge = match (o.affiliation.as_str(), o.role.as_str()) {
                ("owner", _) => t!("role.owner"),
                ("admin", _) => t!("role.admin"),
                (_, "moderator") => t!("role.moderator"),
                (_, "visitor") => t!("role.visitor"),
                _ => "",
            };
            let mine = me.as_ref().is_some_and(|m| m.nick == o.nick);
            // A moderator cannot act on owners/admins (XEP-0045 §8).
            let above = o.affiliation == "owner" || o.affiliation == "admin";
            let (kick, ban, promote) = (is_mod && !mine && !above, is_admin && !mine && !above && o.jid.is_some(), is_admin && !mine && !above);
            let (n1, n3, jid, is_moderator) = (o.nick.clone(), o.nick.clone(), o.jid.clone(), o.role == "moderator");
            h_flex()
                .gap_2()
                .py_1()
                .items_center()
                .child(div().flex_1().min_w_0().truncate().text_sm().text_color(sender_color(&o.nick)).child(o.nick.clone()))
                .when(!badge.is_empty(), |d| d.child(div().text_xs().text_color(p.muted).child(badge)))
                .when(promote, |d| {
                    d.child(
                        Button::new(SharedString::from(format!("mod-{}", o.nick)))
                            .xsmall()
                            .ghost()
                            .label(if is_moderator { t!("role.revoke_mod") } else { t!("role.make_mod") })
                            .on_click(cx.listener(move |this, _, _, cx| this.muc_admin(Some(n3.clone()), None, Some(if is_moderator { "participant" } else { "moderator" }), None, cx))),
                    )
                })
                .when(kick, |d| {
                    d.child(Button::new(SharedString::from(format!("kick-{}", o.nick))).xsmall().ghost().label(t!("role.kick")).on_click(cx.listener(move |this, _, _, cx| this.muc_admin(Some(n1.clone()), None, Some("none"), None, cx))))
                })
                .when(ban, |d| {
                    d.child(
                        Button::new(SharedString::from(format!("ban-{}", o.nick)))
                            .xsmall()
                            .ghost()
                            .label(t!("block.block"))
                            .on_click(cx.listener(move |this, _, _, cx| this.muc_admin(None, jid.clone(), None, Some("outcast"), cx))),
                    )
                })
        });
        v_flex()
            .w_full()
            .pt_3()
            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).text_color(p.muted).child(t!("room.members")))
            .child(v_flex().id("members").max_h(dp(220.)).overflow_y_scroll().children(rows))
            .into_any_element()
    }

    fn render_pgp_import(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let Some(Modal::PgpImport { jid }) = self.modal.as_ref() else { return None };
        let title = if jid.is_some() { t!("pgp.peer_key_title") } else { t!("pgp.import_secret_title") };
        let hint = if jid.is_some() {
            t!("pgp.peer_key_hint")
        } else {
            t!("pgp.secret_hint")
        };
        let t = cx.theme().clone();
        let body = v_flex()
            .gap_3()
            .child(div().text_xs().text_color(t.muted_foreground).child(hint))
            .child(Textarea::new(&self.field3))
            .child(h_flex().justify_end().child(Button::new("pgp-import-ok").primary().label(t!("pgp.import")).on_click(cx.listener(|this, _, _, cx| this.submit_pgp_import(cx)))));
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
            .child(tab_btn("tab-photos", t!("media.tab_photos", n = photos.len()), MediaTab::Photos))
            .child(tab_btn("tab-files", t!("media.tab_files", n = files.len()), MediaTab::Files))
            .child(tab_btn("tab-links", t!("media.tab_links", n = urls.len()), MediaTab::Links));
        let empty = |text: &'static str| div().py_8().text_center().text_sm().text_color(t.muted_foreground).child(text).into_any_element();
        let content = match tab {
            MediaTab::Photos if photos.is_empty() => empty(t!("media.no_photos")),
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
            MediaTab::Files if files.is_empty() => empty(t!("media.no_files")),
            MediaTab::Files => v_flex()
                .gap_1()
                .children(files.into_iter().map(|m| {
                    let url = m.url.clone().unwrap_or_default();
                    let name = file_name(&url);
                    let path = self.sess.as_ref().and_then(|s| s.store.file_for(&url)).filter(|p| p.is_file());
                    let size = path.as_ref().and_then(|p| std::fs::metadata(p).ok()).map(|md| human_size(md.len())).unwrap_or_else(|| t!("media.not_downloaded").into());
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
            MediaTab::Links if urls.is_empty() => empty(t!("media.no_links")),
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
        let title = t!("media.title", name = self.sess.as_ref().map(|s| s.display_name(&peer)).unwrap_or_default());
        Some(self.panel("media", &title, v_flex().child(tabs).child(content).into_any_element(), cx))
    }

    /// Right-click menu over the image viewer; drawn at the root, where window coordinates apply.
    fn render_viewer_menu(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let pos = self.viewer_menu?;
        let Some(Viewer::Image { path, url, .. }) = self.viewer.as_ref() else { return None };
        let (path, url) = (path.clone(), url.clone());
        let p = style::palette(cx);
        let entry = |id: &'static str, icon: IconName, label: &'static str| {
            h_flex().id(id).gap_3().px_3().py_1p5().rounded_md().items_center().cursor_pointer().text_sm().text_color(p.text).hover(|d| d.bg(p.hover)).child(Icon::new(icon).small()).child(label)
        };
        let menu = v_flex()
            .id("viewer-menu")
            .occlude()
            .absolute()
            .left(pos.x)
            .top(pos.y)
            .w(dp(240.))
            .p_1()
            .rounded_lg()
            .bg(p.panel)
            .border_1()
            .border_color(p.divider)
            .shadow_xl()
            .child(entry("vm-copy", IconName::Copy, t!("menu.copy_image")).on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.copy_image(&path, cx);
            })))
            .child(entry("vm-save", IconName::Download, t!("menu.save_as")).on_click(cx.listener(move |this, _, w, cx| {
                cx.stop_propagation();
                this.viewer_menu = None;
                this.save_attachment_as(url.clone(), w, cx);
            })));
        Some(menu.into_any_element())
    }

    fn drag_bar(&self, id: &'static str, value: f32, cx: &mut Context<Self>, set: fn(&mut App, f32, bool, &mut Window, &mut Context<App>)) -> Stateful<Div> {
        let shown = match self.video_drag {
            Some((d, v)) if d == id => v,
            _ => value,
        };
        let frac = |b: Bounds<Pixels>, x: Pixels| if b.size.width > px(0.) { ((x - b.left()) / b.size.width).clamp(0., 1.) } else { 0. };
        let bounds = Rc::new(Cell::new(Bounds::default()));
        let entity = cx.entity().downgrade();
        let track = {
            let bounds = bounds.clone();
            canvas(
                move |b: Bounds<Pixels>, _, _| bounds.set(b),
                move |b, _, window, _| {
                    let e = entity.clone();
                    window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, cx| {
                        if phase == DispatchPhase::Bubble && ev.pressed_button == Some(MouseButton::Left) {
                            let _ = e.update(cx, |app, cx| {
                                if app.video_drag.is_some_and(|(d, _)| d == id) {
                                    let v = frac(b, ev.position.x);
                                    app.video_drag = Some((id, v));
                                    set(app, v, false, window, cx);
                                }
                            });
                        }
                    });
                    window.on_mouse_event(move |ev: &MouseUpEvent, phase, window, cx| {
                        if phase == DispatchPhase::Bubble && ev.button == MouseButton::Left {
                            let _ = entity.update(cx, |app, cx| {
                                if app.video_drag.is_some_and(|(d, _)| d == id) {
                                    app.video_drag = None;
                                    set(app, frac(b, ev.position.x), true, window, cx);
                                }
                            });
                        }
                    });
                },
            )
            .absolute()
            .inset_0()
        };
        div()
            .id(id)
            .h(px(16.))
            .relative()
            .flex()
            .items_center()
            .cursor_pointer()
            .child(div().h(px(4.)).w_full().rounded_full().bg(hsla(0., 0., 1., 0.3)).child(div().h_full().rounded_full().bg(white()).w(relative(shown))))
            .child(track)
            .child(div().absolute().top(px(2.)).left(relative(shown)).ml(px(-6.)).size(px(12.)).rounded_full().bg(white()).shadow_sm())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let v = frac(bounds.get(), ev.position.x);
                    this.video_drag = Some((id, v));
                    set(this, v, false, window, cx);
                }),
            )
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
                    .on_mouse_down(MouseButton::Right, cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                        this.viewer_menu = Some(ev.position);
                        cx.notify();
                    }))
                    .child(if path.is_file() {
                        img(path.clone()).absolute().inset_0().size_full().object_fit(ObjectFit::Contain).into_any_element()
                    } else {
                        div().size_full().flex().items_center().justify_center().text_color(white()).child(t!("viewer.loading")).into_any_element()
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
                    Some(i) if gallery.len() > 1 => t!("viewer.counter", name = file_name(url), n = i + 1, total = gallery.len()),
                    _ => file_name(url),
                };
                (title, path.clone(), body.into_any_element())
            }
            Viewer::Video { path, thumb, size, duration, play, frame, .. } => {
                dims = Some((size.0 as f32, size.1 as f32));
                let total = *duration;
                let pos = match self.video_drag {
                    Some(("video-seek", f)) => f as f64 * total,
                    _ => play.as_ref().map_or(total, |p| p.position().min(total)),
                };
                let paused = play.as_ref().is_none_or(|p| p.paused());
                let clock = |s: f64| format!("{}:{:02}", s as u32 / 60, s as u32 % 60);
                let (volume, muted) = (self.video_volume, self.video_muted);
                let vol_bar = self
                    .drag_bar("video-volume", if muted { 0. } else { volume }, cx, |app, v, _, _, cx| app.video_set_volume(v, false, cx))
                    .w(dp(80.))
                    .flex_shrink_0();
                let picture = match frame {
                    Some(f) => img(f.clone()),
                    None => img(thumb.clone()),
                };
                let seek = self
                    .drag_bar("video-seek", if total > 0. { (pos / total) as f32 } else { 0. }, cx, |app, f, release, w, cx| {
                        let Some(Viewer::Video { duration, .. }) = app.viewer.as_ref() else { return };
                        if release {
                            let to = f as f64 * *duration;
                            app.video_seek(to, w, cx);
                        } else {
                            cx.notify();
                        }
                    })
                    .flex_1();
                let controls = h_flex()
                    .id("video-controls")
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .right_0()
                    .gap_3()
                    .px_3()
                    .py_2()
                    .items_center()
                    .rounded_md()
                    .text_color(white())
                    .bg(hsla(0., 0., 0., 0.55))
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(overlay_icon("video-play", if paused { IconName::Play } else { IconName::Pause }).on_click(cx.listener(|this, _, w, cx| {
                        cx.stop_propagation();
                        this.video_toggle(w, cx)
                    })))
                    .child(div().text_sm().child(format!("{} / {}", clock(pos), clock(total))))
                    .child(seek)
                    .child(overlay_icon("video-mute", if muted { IconName::VolumeX } else { IconName::Volume2 }).on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.video_set_volume(volume, !muted, cx)
                    })))
                    .child(vol_bar);
                let body = div().size_full().relative().child(picture.absolute().inset_0().size_full().object_fit(ObjectFit::Contain)).child(controls);
                let title = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                (title, path.clone(), body.into_any_element())
            }
            Viewer::Text { name, path, text } => {
                let shown: String = text.chars().take(200_000).collect::<String>().replace('\t', "    ");
                // The language comes from the file extension; unknown ones stay plain.
                let lang = name.rsplit_once('.').map(|(_, ext)| lang_id(ext)).unwrap_or("text");
                let hl = syntax_styles(cx.theme().is_dark(), lang, &shown);
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
                    .child(StyledText::new(SharedString::from(shown)).with_highlights((*hl).clone()))
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
                .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                    if this.viewer_menu.take().is_some() {
                        cx.notify();
                    } else if !content.get().contains(&ev.position()) {
                        this.viewer = None;
                        cx.notify();
                    } else if matches!(this.viewer, Some(Viewer::Video { .. })) {
                        this.video_toggle(window, cx);
                    }
                }))
                .on_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                    match ev.keystroke.key.as_str() {
                        "escape" => {
                            this.viewer = None;
                            cx.notify();
                        }
                        "left" => {
                            this.viewer_step(-1, cx);
                            this.video_nudge(-5., window, cx);
                        }
                        "right" => {
                            this.viewer_step(1, cx);
                            this.video_nudge(5., window, cx);
                        }
                        "space" => this.video_toggle(window, cx),
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

    fn render_update(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let rel = self.update.as_ref().filter(|_| self.update_popup)?;
        let t = cx.theme().clone();
        let p = style::palette(cx);
        let notes: Vec<Div> = rel.notes.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with("**Full Changelog**")).map(|l| div().text_sm().child(l.to_string())).collect();
        let action = if self.updating {
            Button::new("update-go").primary().label(t!("update.downloading", percent = self.update_percent())).disabled(true)
        } else {
            Button::new("update-go").primary().label(t!("update.install")).on_click(cx.listener(|this, _, _, cx| this.install_update(cx)))
        };
        Some(
            div()
                .id("update-overlay")
                .occlude()
                .absolute()
                .inset_0()
                .flex()
                .items_center()
                .justify_center()
                .bg(hsla(0., 0., 0., 0.5))
                .on_click(cx.listener(|this, _, _, cx| this.show_update(false, cx)))
                .child(
                    v_flex()
                        .id("update")
                        .w(dp(460.))
                        .p_5()
                        .gap_4()
                        .rounded_lg()
                        .bg(t.background)
                        .border_1()
                        .border_color(t.border)
                        .on_click(|_, _, cx| cx.stop_propagation())
                        .child(
                            v_flex()
                                .gap_1()
                                .child(div().text_lg().font_weight(FontWeight::SEMIBOLD).child(t!("update.available")))
                                .child(div().text_sm().text_color(t.muted_foreground).child(t!("update.version_line", version = rel.version, current = env!("CARGO_PKG_VERSION")))),
                        )
                        .when(!notes.is_empty(), |d| {
                            d.child(
                                v_flex()
                                    .gap_2()
                                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(t!("update.whats_new")))
                                    .child(v_flex().id("update-notes").max_h(dp(280.)).overflow_y_scroll().gap_1().children(notes)),
                            )
                        })
                        .when(self.updating, |d| {
                            d.child(div().h(px(4.)).w_full().rounded_full().bg(hsla(0., 0., 0.5, 0.3)).child(div().h_full().rounded_full().bg(p.accent).w(relative(self.update_percent() as f32 / 100.))))
                        })
                        .when_some(self.update_error.clone(), |d, e| d.child(div().text_sm().text_color(p.danger).child(t!("update.failed", e = e))))
                        .child(
                            h_flex()
                                .gap_2()
                                .justify_end()
                                .child(Button::new("update-later").ghost().label(t!("update.later")).on_click(cx.listener(|this, _, _, cx| this.show_update(false, cx))))
                                .child(action),
                        ),
                )
                .into_any_element(),
        )
    }

    fn render_modal(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let modal = self.modal.as_ref().filter(|m| !matches!(m, Modal::Settings | Modal::Media { .. } | Modal::Encryption { .. } | Modal::PgpImport { .. } | Modal::Profile { .. }))?;
        let t = cx.theme().clone();
        let sess = self.sess.as_ref()?;
        let (title, body): (&str, AnyElement) = match modal {
            Modal::AddContact => (t!("modal.add_contact"), v_flex().gap_2().child(Input::new(&self.field1)).child(Input::new(&self.field2)).into_any_element()),
            Modal::NewGroup => (t!("group.new"), Input::new(&self.field2).into_any_element()),
            Modal::JoinGroup => (t!("modal.join_group"), Input::new(&self.field2).into_any_element()),
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
                (t!("modal.invite"), v_flex().id("invite-list").max_h(dp(320.)).overflow_y_scroll().children(contacts).into_any_element())
            }
            Modal::ScreenPicker { targets } => {
                use crabber::screens::ScreenTarget;
                let section = |title: &'static str| div().w_full().pt_2().pb_1().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(t.muted_foreground).child(title);
                // A preview card: the picture once captured, an icon until then.
                let card = |ix: usize, target: &ScreenTarget, monitor_no: usize| {
                    let (icon, label, detail) = match target {
                        ScreenTarget::Monitor { width, height, primary, .. } => (
                            IconName::Monitor,
                            t!("call.monitor", n = monitor_no),
                            format!("{width}×{height}{}", if *primary { format!(" · {}", t!("call.primary")) } else { String::new() }),
                        ),
                        ScreenTarget::Window { title, .. } => (IconName::AppWindow, title.clone(), String::new()),
                    };
                    let target = target.clone();
                    v_flex()
                        .id(("share", ix))
                        .w(dp(204.))
                        .gap_1()
                        .p_1()
                        .rounded_md()
                        .cursor_pointer()
                        .hover(|d| d.bg(t.accent))
                        .child(
                            div()
                                .w_full()
                                .h(dp(112.))
                                .rounded_md()
                                .overflow_hidden()
                                .bg(t.muted)
                                .flex()
                                .items_center()
                                .justify_center()
                                .child(match self.share_thumbs.get(&ix) {
                                    Some(pic) => img(pic.clone()).size_full().object_fit(ObjectFit::Contain).into_any_element(),
                                    None => Icon::new(icon).size(dp(28.)).text_color(t.muted_foreground).into_any_element(),
                                }),
                        )
                        .child(h_flex().gap_1().text_sm().child(div().flex_1().min_w_0().truncate().child(label)).child(div().text_xs().text_color(t.muted_foreground).child(detail)))
                        .on_click(cx.listener(move |this, _, _, cx| this.share_target(target.clone(), cx)))
                };
                let monitors = targets.iter().filter(|x| matches!(x, ScreenTarget::Monitor { .. })).count();
                let list = v_flex()
                    .id("share-list")
                    .max_h(dp(480.))
                    .overflow_y_scroll()
                    .child(section(t!("call.screens")))
                    .child(h_flex().flex_wrap().gap_1().children(targets.iter().enumerate().take(monitors).map(|(ix, x)| card(ix, x, ix + 1))))
                    .when(targets.len() > monitors, |d| d.child(section(t!("call.windows"))))
                    .child(h_flex().flex_wrap().gap_1().children(targets.iter().enumerate().skip(monitors).map(|(ix, x)| card(ix, x, 0))));
                (t!("call.share_title"), list.into_any_element())
            }
            Modal::ConfirmRetract { moderate, .. } => {
                let text = if *moderate { t!("modal.retract_mod_text") } else { t!("modal.retract_text") };
                (t!("menu.retract"), div().child(text).into_any_element())
            }
            Modal::ConfirmDelete { peer } => {
                let room = sess.chat(peer).is_some_and(|c| c.is_room);
                let text = if room { t!("modal.leave_text") } else { t!("modal.delete_text") };
                (t!("modal.confirm"), div().child(text).into_any_element())
            }
            Modal::Settings | Modal::Media { .. } | Modal::Encryption { .. } | Modal::PgpImport { .. } | Modal::Profile { .. } => return None,
        };
        let confirm = match modal {
            Modal::Invite { .. } | Modal::ScreenPicker { .. } => None,
            Modal::ConfirmRetract { msg, moderate } => {
                let (msg, moderate) = (msg.clone(), *moderate);
                Some(Button::new("modal-ok").danger().label(t!("modal.delete")).on_click(cx.listener(move |this, _, _, cx| this.retract(&msg, moderate, cx))))
            }
            Modal::ConfirmDelete { peer } => {
                let p = peer.clone();
                Some(Button::new("modal-ok").danger().label(t!("modal.delete")).on_click(cx.listener(move |this, _, _, cx| this.delete_chat(p.clone(), cx))))
            }
            _ => Some(Button::new("modal-ok").primary().label(t!("modal.done")).on_click(cx.listener(|this, _, w, cx| this.submit_modal(w, cx)))),
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
                        .w(dp(if matches!(modal, Modal::ScreenPicker { .. }) { 700. } else { 420. }))
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
                                .child(Button::new("modal-cancel").ghost().label(t!("modal.cancel")).on_click(cx.listener(|this, _, _, cx| this.close_modal(cx))))
                                .children(confirm),
                        ),
                )
                .into_any_element(),
        )
    }

    /// Incoming call: a card over the window, since its chat may not be open.
    fn render_call(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sess = self.sess.as_ref()?;
        let call = sess.call.as_ref().filter(|c| c.state == CallState::Incoming)?;
        let p = style::palette(cx);
        let name = sess.display_name(&call.peer);
        let round = |id: &'static str, icon: IconName, bg: Hsla| {
            div().id(id).size(dp(52.)).rounded_full().bg(bg).cursor_pointer().flex().items_center().justify_center().child(Icon::new(icon).size(dp(22.)).text_color(white()))
        };
        // Answering opens the caller's chat, where the call panel lives.
        let accept = |video: bool| {
            let peer = call.peer.clone();
            cx.listener(move |this: &mut App, _: &ClickEvent, w: &mut Window, cx: &mut Context<App>| {
                this.call(CallCmd::Accept { video }, cx);
                if this.sess.as_ref().is_some_and(|s| s.selected.as_ref() != Some(&peer)) {
                    this.select(peer.clone(), w, cx);
                }
            })
        };
        let buttons = h_flex()
            .pt_2()
            .gap_6()
            .child(round("call-accept", IconName::Phone, p.online).on_click(accept(false)))
            .when(call.video, |d| d.child(round("call-accept-video", IconName::Video, p.online).on_click(accept(true))))
            .child(round("call-hangup", IconName::PhoneOff, p.danger).on_click(cx.listener(|this, _, _, cx| this.call(CallCmd::Hangup, cx))));
        let card_bg = linear_gradient(170., linear_color_stop(rgb(0x2B5278), 0.), linear_color_stop(rgb(0x17212B), 1.));
        let status = if call.video { t!("call.incoming_video") } else { t!("call.incoming") };
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
            .child(buttons);
        // Only the card occludes: the rest of the window stays clickable during a call.
        Some(div().absolute().inset_0().flex().items_center().justify_center().child(card).into_any_element())
    }

    /// While another chat (or none) is open: a strip that leads back to the call.
    fn render_call_strip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sess = self.sess.as_ref()?;
        let call = sess.call.as_ref().filter(|c| c.state != CallState::Incoming && sess.selected.as_ref() != Some(&c.peer))?;
        let p = style::palette(cx);
        let peer = call.peer.clone();
        Some(
            h_flex()
                .id("call-strip")
                .flex_shrink_0()
                .h(dp(36.))
                .px_4()
                .gap_2()
                .items_center()
                .cursor_pointer()
                .bg(p.online)
                .text_color(white())
                .text_sm()
                .child(Icon::new(IconName::Phone).size(dp(16.)))
                .child(div().flex_1().min_w_0().truncate().child(format!("{} · {}", sess.display_name(&call.peer), call_status(call))))
                .child(
                    div()
                        .id("call-strip-hangup")
                        .size(dp(28.))
                        .rounded_full()
                        .bg(p.danger)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(Icon::new(IconName::PhoneOff).size(dp(14.)))
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.call(CallCmd::Hangup, cx);
                        })),
                )
                .on_click(cx.listener(move |this, _, w, cx| this.select(peer.clone(), w, cx)))
                .into_any_element(),
        )
    }

    /// While another chat (or none) is open during a group call: a strip back to its room.
    fn render_group_call_strip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sess = self.sess.as_ref()?;
        let g = sess.group_call.as_ref().filter(|g| sess.selected.as_ref() != Some(&g.room))?;
        let p = style::palette(cx);
        let room = g.room.clone();
        Some(
            h_flex()
                .id("group-call-strip")
                .flex_shrink_0()
                .h(dp(36.))
                .px_4()
                .gap_2()
                .items_center()
                .cursor_pointer()
                .bg(p.online)
                .text_color(white())
                .text_sm()
                .child(Icon::new(IconName::Users).size(dp(16.)))
                .child(div().flex_1().min_w_0().truncate().child(format!("{} · {}", sess.display_name(&g.room), group_status(g))))
                .child(
                    div()
                        .id("group-call-strip-leave")
                        .size(dp(28.))
                        .rounded_full()
                        .bg(p.danger)
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(Icon::new(IconName::PhoneOff).size(dp(14.)))
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.group_call(GroupCmd::Leave, cx);
                        })),
                )
                .on_click(cx.listener(move |this, _, w, cx| this.select(room.clone(), w, cx)))
                .into_any_element(),
        )
    }

    /// Group call over the room's chat: a tile per participant (us first), green while talking,
    /// faded until connected; microphone, sound, chat and leave under them.
    fn render_group_call_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sess = self.sess.as_ref()?;
        let g = sess.group_call.as_ref()?;
        let p = style::palette(cx);
        let (fg, soft) = (p.text, p.hover);
        let me = sess.chat(&g.room).map(|c| c.nick.clone()).filter(|n| !n.is_empty()).unwrap_or_else(|| sess.localpart());
        // (avatar JID, name, muted, speaking, connected)
        let mut people = vec![(sess.me.clone(), me, g.muted, g.speaking, true)];
        people.extend(g.peers.iter().map(|x| (x.jid.clone(), x.nick.clone(), x.muted, x.speaking, x.connected)));
        let gap = 12.;
        let stage_size = self.call_stage.get();
        let (w, h) = tile_size((f32::from(stage_size.width), f32::from(stage_size.height)), people.len(), gap);
        let tiles = people.into_iter().map(|(jid, label, muted, speaking, connected)| {
            div()
                .relative()
                .flex_none()
                .w(px(w))
                .h(px(h))
                .rounded(dp(12.))
                .overflow_hidden()
                .bg(p.panel)
                .border_2()
                .border_color(if speaking { p.online } else { transparent_black() })
                .flex()
                .items_center()
                .justify_center()
                .when(!connected, |d| d.opacity(0.5))
                .child(avatar(sess, &jid, &label, 72.))
                .child(
                    h_flex()
                        .absolute()
                        .left(dp(8.))
                        .bottom(dp(8.))
                        .gap_1()
                        .items_center()
                        .px_2()
                        .py(dp(2.))
                        .rounded_md()
                        .bg(p.chip)
                        .text_color(p.chip_text)
                        .text_xs()
                        .child(label)
                        .when(!connected, |d| d.child(format!("· {}", t!("gcall.connecting"))))
                        .when(muted, |d| d.child(Icon::new(IconName::MicOff).size(dp(12.)).text_color(p.danger))),
                )
        });
        // The tiles are sized from the stage, measured while drawing (a frame late on resize).
        let measured = self.call_stage.clone();
        let stage = div().flex_1().min_h_0().p_3().child(
            div()
                .size_full()
                .relative()
                .child(
                    canvas(
                        move |bounds, window, _| {
                            if measured.get() != bounds.size {
                                measured.set(bounds.size);
                                window.refresh();
                            }
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                )
                .child(h_flex().size_full().flex_wrap().gap(px(gap)).justify_center().items_center().content_center().children(tiles)),
        );

        let round = |id: &'static str, icon: IconName, fill: Option<(Hsla, Hsla)>, alert: bool| {
            let icon_color = fill.map_or(if alert { p.danger } else { fg }, |(_, c)| c);
            div()
                .id(id)
                .size(dp(40.))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .map(|d| match fill {
                    Some((bg, _)) => d.bg(bg).hover(|d| d.opacity(0.85)),
                    None => d.hover(move |d| d.bg(soft)),
                })
                .child(Icon::new(icon).size(dp(20.)).text_color(icon_color))
        };
        let pill = |main: Stateful<Div>, menu: AnyElement, alert: bool| {
            h_flex().items_center().p(dp(3.)).gap(dp(2.)).rounded_full().bg(if alert { p.danger.opacity(0.18) } else { p.panel }).child(main).child(menu)
        };
        let this = cx.entity().downgrade();
        let colors = (fg, soft);
        let open = |id: &'static str| self.call_menu == Some(id);
        let mic_menu = {
            let (this, current, denoise, vol) = (this.clone(), self.settings.mic.clone(), self.settings.denoise, self.mic_volume.clone());
            menu_chevron("gcall-mic-menu", this.clone(), open("gcall-mic-menu"), colors, cx, move |menu, window, cx| {
                let (this2, this3, current) = (this.clone(), this.clone(), current.clone());
                menu.submenu(t!("call.input_device"), window, cx, move |m, _, _| device_items(m, DeviceKind::Mic, current.clone(), this2.clone()))
                    .item(check_item(t!("settings.denoise"), denoise).on_click(move |_, w, cx| {
                        let _ = this3.update(cx, |app, cx| app.update_settings(w, cx, |st| st.denoise = !denoise));
                    }))
                    .separator()
                    .item(volume_item(t!("call.mic_volume"), vol.clone()))
            })
        };
        let sound_menu = {
            let (this, current, vol) = (this.clone(), self.settings.speaker.clone(), self.speaker_volume.clone());
            menu_chevron("gcall-sound-menu", this.clone(), open("gcall-sound-menu"), colors, cx, move |menu, window, cx| {
                let (this, current) = (this.clone(), current.clone());
                menu.submenu(t!("call.output_device"), window, cx, move |m, _, _| device_items(m, DeviceKind::Speaker, current.clone(), this.clone()))
                    .separator()
                    .item(volume_item(t!("call.speaker_volume"), vol.clone()))
            })
        };
        let (muted, deafened) = (g.muted, g.deafened);
        let tip = |label: &'static str| move |w: &mut Window, cx: &mut gpui_kit::App| Tooltip::new(label).build(w, cx);
        let controls = h_flex()
            .flex_shrink_0()
            .pb_3()
            .gap_3()
            .items_center()
            .justify_center()
            .child(pill(
                round("gcall-mute", if muted { IconName::MicOff } else { IconName::Mic }, None, muted)
                    .tooltip(tip(t!("call.mic")))
                    .on_click(cx.listener(move |this, _, _, cx| this.group_call(GroupCmd::Mute(!muted), cx))),
                mic_menu.into_any_element(),
                muted,
            ))
            .child(pill(
                round("gcall-deafen", if deafened { IconName::HeadphoneOff } else { IconName::Headphones }, None, deafened)
                    .tooltip(tip(t!("call.sound")))
                    .on_click(cx.listener(move |this, _, _, cx| this.group_call(GroupCmd::Deafen(!deafened), cx))),
                sound_menu.into_any_element(),
                deafened,
            ))
            .child(
                round("gcall-chat", if self.call_chat_hidden { IconName::MessageSquareOff } else { IconName::MessageSquare }, Some((p.panel, fg)), false)
                    .tooltip(tip(t!("call.chat")))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.call_chat_hidden = !this.call_chat_hidden;
                        cx.notify();
                    })),
            )
            .child(
                round("gcall-leave", IconName::PhoneOff, Some((p.danger, white())), false)
                    .tooltip(tip(t!("gcall.leave")))
                    .on_click(cx.listener(|this, _, _, cx| this.group_call(GroupCmd::Leave, cx))),
            );
        let header = h_flex()
            .flex_shrink_0()
            .px_4()
            .pt_3()
            .gap_2()
            .items_center()
            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(sess.display_name(&g.room)))
            .child(div().text_sm().text_color(p.muted).child(group_status(g)));
        Some(v_flex().id("group-call-panel").size_full().bg(p.chat_top).text_color(fg).child(header).child(stage).child(controls).into_any_element())
    }

    /// Discord-like call stage over the chat: both sides as 16:9 tiles, the controls under them.
    /// A clicked tile fills the stage (the other one stays in the corner); the panel can also go
    /// over the whole screen.
    fn render_call_panel(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let sess = self.sess.as_ref()?;
        let call = sess.call.as_ref().filter(|c| c.state != CallState::Incoming)?;
        let p = style::palette(cx);
        let name = sess.display_name(&call.peer);
        let me = sess.localpart();
        let (fg, soft) = (p.text, p.hover);

        // `peer`: the other side's tile, else ours.
        let tile = |peer: bool| {
            let (pic, jid, label, muted) = match peer {
                true => (call.remote.clone(), call.peer.as_str(), name.clone(), call.peer_muted),
                false => (call.local.clone(), sess.me.as_str(), me.clone(), call.muted),
            };
            let speaking = call.speaking[peer as usize];
            let shows = pic.is_some();
            div()
                .id(if peer { "tile-peer" } else { "tile-me" })
                .relative()
                .flex_none()
                .rounded(dp(12.))
                .overflow_hidden()
                .bg(p.panel)
                .border_2()
                .border_color(if speaking { p.online } else { transparent_black() })
                .flex()
                .items_center()
                .justify_center()
                .when(shows, |d| d.cursor_pointer())
                .child(match pic {
                    Some(pic) => img(pic).size_full().object_fit(ObjectFit::Contain).into_any_element(),
                    None => avatar(sess, jid, &label, 72.),
                })
                .child(
                    h_flex()
                        .absolute()
                        .left(dp(8.))
                        .bottom(dp(8.))
                        .gap_1()
                        .items_center()
                        .px_2()
                        .py(dp(2.))
                        .rounded_md()
                        .bg(p.chip)
                        .text_color(p.chip_text)
                        .text_xs()
                        .child(label)
                        .when(muted, |d| d.child(Icon::new(IconName::MicOff).size(dp(12.)).text_color(p.danger))),
                )
        };
        // Only a picture is worth enlarging: a tile without one isn't clickable, and one that lost
        // its picture goes back into the grid.
        let shows = |peer: bool| if peer { call.remote.is_some() } else { call.local.is_some() };
        let focused = self.call_focus.filter(|&f| shows(f));
        // An expanded panel hides its bars after a while without the mouse moving.
        let idle = self.call_expanded && self.call_menu.is_none() && self.call_mouse.elapsed().as_secs() >= 3;
        // Expanded with one tile in focus, the picture takes the whole window, edge to edge.
        let edge = self.call_expanded && focused.is_some();
        let gap = 12.;
        let stage_size = self.call_stage.get();
        let content = match focused {
            None => {
                let (w, h) = tile_size((f32::from(stage_size.width), f32::from(stage_size.height)), 2, gap);
                h_flex()
                    .size_full()
                    .flex_wrap()
                    .gap(px(gap))
                    .justify_center()
                    .items_center()
                    .content_center()
                    .children([true, false].map(|peer| {
                        tile(peer).w(px(w)).h(px(h)).when(shows(peer), |d| {
                            d.on_click(cx.listener(move |this, _, _, cx| this.set_call_focus(Some(peer), cx)))
                        })
                    }))
                    .into_any_element()
            }
            Some(focus) => div()
                .size_full()
                .relative()
                .child(
                    tile(focus)
                        .size_full()
                        .when(edge, |d| d.rounded_none().border_0().bg(black()))
                        .on_click(cx.listener(|this, _, _, cx| this.set_call_focus(None, cx))),
                )
                .when(!self.call_hide_others, |d| {
                    d.child(
                    tile(!focus)
                        .absolute()
                        .right(dp(12.))
                        .bottom(dp(12.))
                        .w(dp(192.))
                        .h(dp(108.))
                        .shadow_lg()
                        .when(shows(!focus), |d| {
                            d.on_click(cx.listener(move |this, _, _, cx| this.set_call_focus(Some(!focus), cx)))
                        }),
                    )
                })
                .into_any_element(),
        };
        // The tiles are sized from the stage, measured while drawing (a frame late on resize).
        let measured = self.call_stage.clone();
        let stage = div().flex_1().min_h_0().when(!edge, |d| d.p_3()).child(
            div()
                .size_full()
                .relative()
                .child(
                    canvas(
                        move |bounds, window, _| {
                            if measured.get() != bounds.size {
                                measured.set(bounds.size);
                                window.refresh();
                            }
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .size_full(),
                )
                .child(content),
        );

        // A round button: `fill` gives it a background and icon colour, else it is see-through;
        // `alert` paints the icon red.
        let round = |id: &'static str, icon: IconName, fill: Option<(Hsla, Hsla)>, alert: bool| {
            let icon_color = fill.map_or(if alert { p.danger } else { fg }, |(_, c)| c);
            div()
                .id(id)
                .size(dp(40.))
                .rounded_full()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .map(|d| match fill {
                    Some((bg, _)) => d.bg(bg).hover(|d| d.opacity(0.85)),
                    None => d.hover(move |d| d.bg(soft)),
                })
                .child(Icon::new(icon).size(dp(20.)).text_color(icon_color))
        };
        let on = Some((p.accent, white()));
        let card = Some((p.panel, fg));
        let this = cx.entity().downgrade();
        let colors = (fg, soft);
        let open = |id: &'static str| self.call_menu == Some(id);
        // A button and its arrow on one pill.
        let pill = |main: Stateful<Div>, menu: AnyElement, alert: bool| {
            h_flex().items_center().p(dp(3.)).gap(dp(2.)).rounded_full().bg(if alert { p.danger.opacity(0.18) } else { p.panel }).child(main).child(menu)
        };

        let (muted, deafened, sending) = (call.muted, call.deafened, call.sending);
        // Video can be turned on once the call is up, or while ringing out a video call.
        let can_video = call.video || matches!(call.state, CallState::Connecting | CallState::Active);
        let toggle_src = move |src: Source| if sending == Some(src) { None } else { Some(src) };

        let mic_menu = {
            let (this, current, denoise, vol) = (this.clone(), self.settings.mic.clone(), self.settings.denoise, self.mic_volume.clone());
            menu_chevron("call-mic-menu", this.clone(), open("call-mic-menu"), colors, cx, move |menu, window, cx| {
                let (this2, this3, current) = (this.clone(), this.clone(), current.clone());
                menu.submenu(t!("call.input_device"), window, cx, move |m, _, _| device_items(m, DeviceKind::Mic, current.clone(), this2.clone()))
                    .item(check_item(t!("settings.denoise"), denoise).on_click(move |_, w, cx| {
                        let _ = this3.update(cx, |app, cx| app.update_settings(w, cx, |st| st.denoise = !denoise));
                    }))
                    .separator()
                    .item(volume_item(t!("call.mic_volume"), vol.clone()))
            })
        };
        let sound_menu = {
            let (this, current, vol) = (this.clone(), self.settings.speaker.clone(), self.speaker_volume.clone());
            menu_chevron("call-sound-menu", this.clone(), open("call-sound-menu"), colors, cx, move |menu, window, cx| {
                let (this, current) = (this.clone(), current.clone());
                menu.submenu(t!("call.output_device"), window, cx, move |m, _, _| device_items(m, DeviceKind::Speaker, current.clone(), this.clone()))
                    .separator()
                    .item(volume_item(t!("call.speaker_volume"), vol.clone()))
            })
        };
        let camera_menu = {
            let (this, current) = (this.clone(), self.settings.camera.clone());
            menu_chevron("call-camera-menu", this.clone(), open("call-camera-menu"), colors, cx, move |menu, _, _| device_items(menu, DeviceKind::Camera, current.clone(), this.clone()))
        };

        // Screen share: another source, a monitor straight away (Windows), quality, system audio.
        let share_menu = {
            let (this, quality, audio) = (this.clone(), self.settings.share_quality, self.settings.share_audio);
            let sharing = sending == Some(Source::Screen);
            menu_chevron("call-share-menu", this.clone(), open("call-share-menu"), colors, cx, move |menu, window, cx| {
                let mut menu = menu;
                if sharing && crate::app::can_pick_screen() {
                    let this = this.clone();
                    menu = menu.item(PopupMenuItem::new(t!("call.share_change")).on_click(move |_, w, cx| {
                        let _ = this.update(cx, |app, cx| app.change_screen(w, cx));
                    }));
                }
                if cfg!(windows) {
                    let current = crabber::videocall::screen();
                    let monitors = crabber::screens::list().into_iter().filter(|x| matches!(x, crabber::screens::ScreenTarget::Monitor { .. }));
                    for (i, m) in monitors.enumerate() {
                        let crabber::screens::ScreenTarget::Monitor { width, height, .. } = &m else { continue };
                        let label = format!("{} · {width}×{height}", t!("call.monitor", n = i + 1));
                        let (this, checked) = (this.clone(), sharing && current.as_ref() == Some(&m));
                        menu = menu.item(PopupMenuItem::new(label).checked(checked).on_click(move |_, _, cx| {
                            let m = m.clone();
                            let _ = this.update(cx, |app, cx| app.share_target(m, cx));
                        }));
                    }
                }
                let this2 = this.clone();
                // Only what the shared screen allows: no 2K of a Full HD one; the original is the largest.
                let height = crabber::videocall::capture_size().filter(|_| sharing).map(|(_, h)| h);
                menu = menu.separator().submenu(t!("call.share_quality"), window, cx, move |mut m, _, _| {
                    use crabber::videocall::Quality;
                    for q in Quality::offered(height) {
                        let this = this2.clone();
                        let label = match (q, height) {
                            (Quality::Source, Some(h)) => format!("{} · {h}p", quality_label(q)),
                            _ => quality_label(q).to_string(),
                        };
                        m = m.item(PopupMenuItem::new(label).checked(q == quality.effective(height)).on_click(move |_, w, cx| {
                            let _ = this.update(cx, |app, cx| app.update_settings(w, cx, |st| st.share_quality = q));
                        }));
                    }
                    m
                });
                if cfg!(any(target_os = "linux", windows)) {
                    let this = this.clone();
                    menu = menu.item(check_item(t!("call.share_audio"), audio).on_click(move |_, w, cx| {
                        let _ = this.update(cx, |app, cx| app.update_settings(w, cx, |st| st.share_audio = !audio));
                    }));
                }
                menu
            })
        };
        let expand_menu = {
            // An option: expanding then takes the whole screen.
            let (this, fullscreen) = (this.clone(), self.settings.call_view.fullscreen);
            menu_chevron("call-expand-menu", this.clone(), open("call-expand-menu"), colors, cx, move |menu, _, _| {
                let this = this.clone();
                menu.item(check_item(t!("call.fullscreen"), fullscreen).on_click(move |_, w, cx| {
                    let _ = this.update(cx, |app, cx| app.set_fullscreen_option(!fullscreen, w, cx));
                }))
            })
        };

        let cam = sending == Some(Source::Camera);
        let screen = sending == Some(Source::Screen);
        let expanded = self.call_expanded;
        let tip = |label: &'static str| move |w: &mut Window, cx: &mut gpui_kit::App| Tooltip::new(label).build(w, cx);
        let controls = h_flex()
            .flex_shrink_0()
            .pb_3()
            .gap_3()
            .items_center()
            .justify_center()
            .child(pill(
                round("call-mute", if muted { IconName::MicOff } else { IconName::Mic }, None, muted)
                    .tooltip(tip(t!("call.mic")))
                    .on_click(cx.listener(move |this, _, _, cx| this.call(CallCmd::Mute(!muted), cx))),
                mic_menu.into_any_element(),
                muted,
            ))
            .child(pill(
                round("call-deafen", if deafened { IconName::HeadphoneOff } else { IconName::Headphones }, None, deafened)
                    .tooltip(tip(t!("call.sound")))
                    .on_click(cx.listener(move |this, _, _, cx| this.call(CallCmd::Deafen(!deafened), cx))),
                sound_menu.into_any_element(),
                deafened,
            ))
            .child(pill(
                round("call-camera", if cam { IconName::Video } else { IconName::VideoOff }, if cam { on } else { None }, false)
                    .tooltip(tip(t!("call.camera")))
                    .when(!can_video, |d| d.opacity(0.4).cursor_default())
                    .when(can_video, |d| d.on_click(cx.listener(move |this, _, _, cx| this.call(CallCmd::Video(toggle_src(Source::Camera)), cx)))),
                camera_menu.into_any_element(),
                false,
            ))
            .child(pill(
                round("call-screen", if screen { IconName::ScreenShare } else { IconName::ScreenShareOff }, if screen { on } else { None }, false)
                    .tooltip(tip(t!("call.screen")))
                    .when(!can_video, |d| d.opacity(0.4).cursor_default())
                    .when(can_video, |d| d.on_click(cx.listener(move |this, _, w, cx| this.share_screen(!screen, w, cx)))),
                share_menu.into_any_element(),
                false,
            ))
            .when(focused.is_some(), |d| {
                let hidden = self.call_hide_others;
                d.child(
                    round("call-others", if hidden { IconName::UserX } else { IconName::Users }, card, false)
                        .tooltip(tip(if hidden { t!("call.show_others") } else { t!("call.hide_others") }))
                        .on_click(cx.listener(move |this, _, _, cx| this.set_call_hide_others(!hidden, cx))),
                )
            })
            .when(!expanded, |d| {
                d.child(
                    round("call-chat", if self.call_chat_hidden { IconName::MessageSquareOff } else { IconName::MessageSquare }, card, false)
                        .tooltip(tip(t!("call.chat")))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.call_chat_hidden = !this.call_chat_hidden;
                            cx.notify();
                        })),
                )
            })
            .child(pill(
                round("call-expand", if expanded { IconName::Minimize } else { IconName::Maximize }, None, false)
                    .tooltip(tip(if expanded { t!("call.collapse") } else { t!("call.expand") }))
                    .on_click(cx.listener(move |this, _, w, cx| this.set_call_expanded(!expanded, w, cx))),
                expand_menu.into_any_element(),
                false,
            ))
            .child(
                round("call-hangup", IconName::PhoneOff, Some((p.danger, white())), false)
                    .tooltip(tip(t!("call.hangup")))
                    .on_click(cx.listener(|this, _, _, cx| this.call(CallCmd::Hangup, cx))),
            );

        let header = h_flex()
            .flex_shrink_0()
            .px_4()
            .pt_3()
            .gap_2()
            .items_center()
            .when(edge, |d| d.text_color(white()))
            .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child(name))
            .child(div().text_sm().text_color(if edge { white().opacity(0.75) } else { p.muted }).child(call_status(call)));
        let panel = v_flex()
            .id("call-panel")
            .size_full()
            .bg(p.chat_top)
            .text_color(fg)
            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                this.call_mouse = std::time::Instant::now();
                if idle {
                    cx.notify();
                }
            }))
            // A click is activity too: on hidden bars it only brings them back (they then take the
            // click's release, so the picture underneath doesn't get it).
            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| {
                this.call_mouse = std::time::Instant::now();
                if idle {
                    cx.notify();
                }
            }));
        // Expanded, the bars float over the stage, so hiding them leaves the tiles where they are.
        Some(if expanded {
            // Over the picture a shade keeps the bars readable; they fade in and out, and catch
            // their own clicks (a click on a button used to reach the tile underneath too).
            let shade = |angle: f32, depth: f32| linear_gradient(angle, linear_color_stop(black().opacity(depth), 0.), linear_color_stop(transparent_black(), 1.));
            let bar = |name: &'static str, d: Div| {
                d.absolute()
                    .left_0()
                    .right_0()
                    .with_animation((name, idle as u64), Animation::new(std::time::Duration::from_millis(if idle { 400 } else { 150 })).with_easing(ease_in_out), move |d, t| {
                        if !idle {
                            d.opacity(t).occlude()
                        } else if t < 1. {
                            d.opacity(1. - t).occlude()
                        } else {
                            d.invisible()
                        }
                    })
            };
            panel
                .relative()
                .when(!edge, |d| d.pt(dp(36.)).pb(dp(56.)))
                .child(stage)
                .child(bar("call-top-bar", div().top_0().when(edge, |d| d.pb(dp(24.)).bg(shade(180., 0.45))).child(header)))
                .child(bar("call-bottom-bar", div().bottom_0().when(edge, |d| d.pt(dp(48.)).bg(shade(0., 0.6))).child(controls)))
                .into_any_element()
        } else {
            panel.child(header).child(stage).child(controls).into_any_element()
        })
    }
}

/// The largest 16:9 tile for `n` tiles laid out in a grid within `area`, `gap` apart: one row
/// on a wide stage, stacked on a tall one.
fn tile_size((width, height): (f32, f32), n: usize, gap: f32) -> (f32, f32) {
    let w = (1..=n)
        .map(|rows| {
            let cols = n.div_ceil(rows) as f32;
            let rows = rows as f32;
            ((width - gap * (cols - 1.)) / cols).min((height - gap * (rows - 1.)) / rows * 16. / 9.)
        })
        .fold(0f32, f32::max)
        .max(16.);
    (w, w * 9. / 16.)
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
    fn call_tiles_fit_the_stage() {
        // Wide stage: side by side, limited by the height.
        assert_eq!(super::tile_size((1000., 200.), 2, 10.), (200. * 16. / 9., 200.));
        // Tall stage: stacked, limited by the width.
        let (w, h) = super::tile_size((400., 900.), 2, 10.);
        assert_eq!(w, 400.);
        assert!(h * 2. + 10. <= 900.);
    }

    #[test]
    fn urls_in_text() {
        let t = "см. https://a.io/x?y=1, и (http://b.ru). не ссылка: http:// ok";
        let found: Vec<&str> = super::url_ranges(t).into_iter().map(|r| &t[r]).collect();
        assert_eq!(found, ["https://a.io/x?y=1", "http://b.ru"]);
    }

    #[test]
    fn quote_lines_split_into_blocks() {
        use super::QuotePart::*;
        let parts = super::split_quotes("обычная\n> цитата\n>вторая строка\nне цитата\n> снова");
        let shown: Vec<&str> = parts
            .iter()
            .map(|p| match p {
                Text(t) => t.as_str(),
                Quote(q) => q.as_str(),
            })
            .collect();
        assert_eq!(shown, ["обычная", "цитата\nвторая строка", "не цитата", "снова"]);
        assert!(matches!(parts[0], Text(_)));
        assert!(matches!(parts[1], Quote(_)));
        assert!(matches!(parts[2], Text(_)));
        assert!(matches!(parts[3], Quote(_)));
    }

    #[test]
    fn language_aliases() {
        assert_eq!(super::lang_id("PY"), "python");
        assert_eq!(super::lang_id("Sh"), "bash");
        assert_eq!(super::lang_id("c++"), "cpp");
        assert_eq!(super::lang_id("rs"), "rust");
        assert_eq!(super::lang_id("json"), "json");
        assert_eq!(super::lang_id("never-heard"), "never-heard");
    }

    #[test]
    fn code_gets_syntax_colors() {
        // A keyword and a string must pick up non-default colors (here: any color at all).
        let styles = super::syntax_styles(true, "rust", "fn main() { let s = \"текст\"; }");
        assert!(styles.iter().any(|(r, s)| r.len() > 0 && s.color.is_some()), "no styles: {styles:?}");
        // The cache returns the same styles for the same input.
        assert!(std::ptr::eq(
            &*super::syntax_styles(true, "rust", "fn main() { let s = \"текст\"; }"),
            &*styles
        ));
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
    fn double_click_word_drag_extends_selection() {
        use gpui_kit::*;
        struct V;
        impl Render for V {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().w(px(400.)).child(super::Selectable::new("t", "aaaa bbbb cccc dddd".to_string(), vec![]))
            }
        }
        let mut cx = TestAppContext::single();
        cx.update(gpui_kit::init);
        let window = cx.add_window(|window, cx| base::Root::new(cx.new(|_| V), window, cx));
        let cx = &mut VisualTestContext::from_window(window.into(), &cx);
        cx.update(|window, cx| drop(window.draw(cx)));
        // Double-click "bbbb" (x≈85), hold and drag onto "cccc" (x≈135): both words get selected.
        cx.simulate_event(MouseDownEvent { position: point(px(85.), px(8.)), modifiers: Modifiers::default(), button: MouseButton::Left, click_count: 2, first_mouse: false });
        cx.simulate_mouse_move(point(px(135.), px(8.)), Some(MouseButton::Left), Modifiers::default());
        cx.simulate_mouse_up(point(px(135.), px(8.)), MouseButton::Left, Modifiers::default());
        cx.update(|window, cx| drop(window.draw(cx)));
        assert_eq!(cx.update(|window, cx| base::TextSelection::selected_text(window, cx)), "bbbb cccc ");
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
    use gpui_kit::{Context, IntoElement, ListAlignment, ListState, ParentElement as _, Render, Styled as _, TestAppContext, Window, black, div, list, px};
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
