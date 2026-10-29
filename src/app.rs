//! UI state and logic. All persistence happens here; the network thread only reports events.
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use futures::StreamExt;
use futures::channel::mpsc::UnboundedSender;
use gpui_kit::component::{
    Theme, ThemeMode, WindowExt as _,
    input::{InputEvent, InputState, TextareaState},
};
use gpui_kit::*;
use crabber::call::{CallCmd, CallEv, CallState};
use crabber::store::{self, ChatRow, Msg, Store};
use crabber::e2e::DeviceInfo;
use crabber::xmpp::{self, Account, Bookmark, Cmd, Ev, Occupant, Reply};
use tokio_xmpp::jid::BareJid;
use tokio_xmpp::parsers::presence::Show;

use crate::desktop::{self, DesktopEvent};

const KEYRING_SERVICE: &str = "rust-ejabber";

#[derive(Clone, Copy, PartialEq)]
pub enum MediaTab {
    Photos,
    Files,
    Links,
}

pub enum Modal {
    Settings,
    /// Encryption mode and device trust of one chat.
    Encryption { peer: String },
    /// Paste an armored OpenPGP key: our secret key (`jid: None`) or a contact's public key.
    PgpImport { jid: Option<String> },
    /// Photos, files and links of one chat.
    Media { peer: String, tab: MediaTab, items: Vec<Msg>, links: Vec<Msg> },
    AddContact,
    NewGroup,
    JoinGroup,
    Invite { room: String },
    ConfirmDelete { peer: String },
    /// Delete a message for everyone; `moderate`: someone else's, as a group moderator.
    ConfirmRetract { msg: Msg, moderate: bool },
    /// Contact / group card opened from the chat header.
    Profile { peer: String },
}

/// Full-window preview of an attachment.
pub enum Viewer {
    /// `gallery`: all image URLs of the chat, for left/right navigation.
    Image { path: PathBuf, url: String, gallery: Vec<String> },
    Text { name: String, path: PathBuf, text: String },
}

/// App-wide preferences (stored in app.db).
#[derive(Clone)]
pub struct Settings {
    /// "system" | "light" | "dark"
    pub theme: String,
    pub notify: bool,
    pub notify_text: bool,
    pub auto_download: bool,
    /// OMEMO Blind Trust Before Verification.
    pub blind_trust: bool,
    /// Interface scale, 1.0 = 100 %.
    pub ui_scale: f32,
    /// Launch at login. Read from the OS (registry / autostart file), not stored in app.db.
    pub autostart: bool,
}

impl Settings {
    fn load(s: &Store) -> Self {
        let flag = |k: &str| s.get(k).is_none_or(|v| v == "1");
        Self {
            theme: s.get("theme").unwrap_or_else(|| "system".into()),
            notify: flag("notify"),
            notify_text: flag("notify_text"),
            auto_download: flag("auto_download"),
            blind_trust: flag("blind_trust"),
            ui_scale: s.get("ui_scale").and_then(|v| v.parse().ok()).filter(|v: &f32| (0.5..=2.0).contains(v)).unwrap_or(1.0),
            autostart: desktop::autostart(),
        }
    }
    fn save(&self, s: &Store) {
        let b = |v: bool| if v { "1" } else { "0" };
        s.set("theme", &self.theme);
        s.set("notify", b(self.notify));
        s.set("notify_text", b(self.notify_text));
        s.set("auto_download", b(self.auto_download));
        s.set("blind_trust", b(self.blind_trust));
        s.set("ui_scale", &self.ui_scale.to_string());
    }
}

pub struct CallUi {
    pub peer: String,
    pub state: CallState,
    pub muted: bool,
    pub started: Option<Instant>,
    pub outgoing: bool,
}

pub struct Session {
    pub me: String,
    pub cmd: UnboundedSender<Cmd>,
    pub store: Store,
    pub dir: PathBuf,
    pub online: bool,
    pub ever_online: bool,
    pub status: SharedString,
    pub show: Option<Show>,
    pub chats: Vec<ChatRow>,
    pub presence: HashSet<String>,
    pub typing: HashMap<String, (String, Instant)>,
    pub occupants: HashMap<String, usize>,
    pub requests: Vec<String>,
    pub avatars: HashMap<String, Arc<Image>>,
    pub selected: Option<String>,
    pub history: Vec<Msg>,
    pub downloads: HashSet<String>,
    /// Downloads that failed (e.g. expired upload); retried on click.
    pub failed: HashSet<String>,
    pub displayed_sent: HashMap<String, String>,
    pub typing_sent: Option<Instant>,
    pub call: Option<CallUi>,
    /// Files chosen or dropped but not sent yet.
    pub pending: Vec<PathBuf>,
    /// Upload id -> local source, copied into the cache once uploaded (no re-download).
    pub uploads: HashMap<String, PathBuf>,
    /// Message to highlight after jumping to it from search.
    pub flash: Option<(String, Instant)>,
    /// OMEMO devices per JID, as last reported by the network thread.
    pub e2e_devices: HashMap<String, Vec<DeviceInfo>>,
    /// Our OpenPGP key: (fingerprint, armored public key).
    pub pgp_own: Option<(String, String)>,
    /// Contact -> (fingerprint of the stored key, key id seen in their presence).
    pub pgp_contacts: HashMap<String, (Option<String>, Option<String>)>,
    /// Our message being corrected in the composer (XEP-0308).
    pub editing: Option<String>,
    /// Reactions of the open chat: msg_id -> (sender, emoji).
    pub reactions: HashMap<String, Vec<(String, String)>>,
    /// Message we are answering in the composer (XEP-0461).
    pub replying: Option<Msg>,
    /// XEP-0490 read marker per chat (stanza-id), from any of our devices.
    pub mds: HashMap<String, String>,
    /// XEP-0191 blocked JIDs; `None` until the server answered (or it has no blocking).
    pub blocked: Option<HashSet<String>>,
    /// Room -> nick -> occupant (roles for moderation).
    pub members: HashMap<String, HashMap<String, Occupant>>,
}

impl Session {
    pub fn send(&self, cmd: Cmd) {
        let _ = self.cmd.unbounded_send(cmd);
    }
    pub fn chat(&self, peer: &str) -> Option<&ChatRow> {
        self.chats.iter().find(|c| c.peer == peer)
    }
    fn chat_mut(&mut self, peer: &str) -> Option<&mut ChatRow> {
        self.chats.iter_mut().find(|c| c.peer == peer)
    }
    pub fn display_name(&self, peer: &str) -> String {
        match self.chat(peer) {
            Some(c) if !c.name.is_empty() => c.name.clone(),
            _ => peer.split('@').next().unwrap_or(peer).to_string(),
        }
    }
    pub fn localpart(&self) -> String {
        self.me.split('@').next().unwrap_or("me").to_string()
    }
    fn domain(&self) -> String {
        self.me.split('@').nth(1).unwrap_or("localhost").to_string()
    }
    /// Creates the chat row if needed and returns it.
    fn ensure_chat(&mut self, peer: &str, is_room: bool) -> &mut ChatRow {
        if self.chat(peer).is_none() {
            let nick = if is_room { self.localpart() } else { String::new() };
            let row = ChatRow { peer: peer.to_string(), is_room, nick, ..Default::default() };
            self.store.save_chat(&row);
            self.chats.push(row);
        }
        self.chat_mut(peer).unwrap()
    }
    fn save(&self, peer: &str) {
        if let Some(c) = self.chat(peer) {
            self.store.save_chat(c);
        }
    }
    fn sort_chats(&mut self) {
        self.chats.sort_by_key(|c| std::cmp::Reverse(c.last.as_ref().map_or(0, |m| m.ts)));
    }
    /// How we appear as a sender in this chat: our nick in groups, our JID otherwise.
    pub fn my_sender(&self, peer: &str) -> String {
        match self.chat(peer) {
            Some(c) if c.is_room => c.nick.clone(),
            _ => self.me.clone(),
        }
    }
    fn publish_bookmark(&self, room: &str) {
        if let Some(c) = self.chat(room) {
            self.send(Cmd::Bookmark(Bookmark { room: room.to_string(), name: c.name.clone(), nick: c.nick.clone() }));
        }
    }
    fn load_avatar(&mut self, jid: &str) {
        let path = store::avatar_path(&self.dir, jid);
        if let Ok(bytes) = std::fs::read(path)
            && let Some(format) = image_format(&bytes)
        {
            self.avatars.insert(jid.to_string(), Arc::new(Image::from_bytes(format, bytes)));
        }
    }
}

pub fn image_format(b: &[u8]) -> Option<ImageFormat> {
    Some(match b {
        [0x89, b'P', b'N', b'G', ..] => ImageFormat::Png,
        [0xFF, 0xD8, ..] => ImageFormat::Jpeg,
        [b'G', b'I', b'F', ..] => ImageFormat::Gif,
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => ImageFormat::Webp,
        [b'B', b'M', ..] => ImageFormat::Bmp,
        _ => return None,
    })
}

/// Writes a pasted image to a temp file so it can be attached like any other file.
fn save_pasted(img: &Image) -> Option<PathBuf> {
    // BMP (plain PrintScreen) is huge and other clients may not show it: re-encode as PNG.
    let (bytes, ext) = if img.format == ImageFormat::Bmp {
        let mut png = vec![];
        ::image::load_from_memory(&img.bytes).ok()?.write_to(&mut std::io::Cursor::new(&mut png), ::image::ImageFormat::Png).ok()?;
        (png, "png")
    } else {
        (img.bytes.clone(), img.format.extension())
    };
    // shortcut: pasted files stay in the OS temp dir after sending; clean up if that ever matters.
    let path = std::env::temp_dir().join(format!("image-{}.{ext}", chrono::Local::now().format("%Y%m%d-%H%M%S-%3f")));
    std::fs::write(&path, bytes).ok()?;
    Some(path)
}

/// Voice messages we can play (Ogg Opus, what Conversations and we record).
pub fn is_audio(url: &str) -> bool {
    matches!(ext(url).as_str(), "oga" | "ogg" | "opus")
}

/// Linux file managers put copied files on the clipboard as `text/uri-list`, which GPUI
/// cannot read back as paths; the same clipboard also carries the paths as plain text.
/// When every line of that text names an existing file, it is a file copy, not a message.
fn clipboard_text_paths(text: &str) -> Option<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let path = if let Some(rest) = line.strip_prefix("file://") {
            if !rest.starts_with('/') {
                return None; // a URI with a host: no local file to attach
            }
            PathBuf::from(percent_decode(rest))
        } else {
            PathBuf::from(line)
        };
        if !path.is_file() {
            return None;
        }
        paths.push(path);
    }
    (!paths.is_empty()).then_some(paths)
}

/// Decodes `%XX` escapes of a file:// URL; anything not a valid escape stays as written.
fn percent_decode(s: &str) -> String {
    fn hex(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(hi), Some(lo)) = (hex(b[i + 1]), hex(b[i + 2]))
        {
            out.push(hi * 16 + lo);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn is_image(url: &str) -> bool {
    // `#` carries the key of aesgcm:// links in encrypted chats.
    let path = url.split(['?', '#']).next().unwrap_or(url).to_ascii_lowercase();
    [".png", ".jpg", ".jpeg", ".gif", ".webp", ".bmp"].iter().any(|e| path.ends_with(e))
}

pub struct App {
    pub login_jid: Entity<InputState>,
    pub login_pass: Entity<InputState>,
    pub login_host: Entity<InputState>,
    pub insecure: bool,
    pub remember: bool,
    pub login_error: SharedString,
    pub connecting: bool,

    pub sess: Option<Session>,
    pub search: Entity<InputState>,
    pub compose: Entity<TextareaState>,
    pub field1: Entity<InputState>,
    pub field2: Entity<InputState>,
    /// Multi-line field for pasting keys.
    pub field3: Entity<TextareaState>,
    pub modal: Option<Modal>,
    pub messages: ListState,
    pub viewer: Option<Viewer>,
    pub viewer_focus: FocusHandle,
    pub settings: Settings,
    pub search_results: Vec<(String, Msg)>,
    /// With a chat open: search all chats instead of just this one.
    pub search_all: bool,
    /// Holds focus when nothing else does, so Esc still reaches the app.
    pub root_focus: FocusHandle,
    /// The app icon, for the title bar and the login screen.
    pub logo: Arc<Image>,
    /// Right-click menu: the message and the window position it was opened at.
    pub msg_menu: Option<(Msg, Point<Pixels>)>,
    /// Full reaction picker: message id, where it opened, the shown emoji group.
    /// Emoji picker: message to react to (`None`: insert into the composer), where it opened, the shown group.
    pub emoji_picker: Option<(Option<String>, Point<Pixels>, emojis::Group)>,
    /// Voice message being recorded, since when.
    pub recording: Option<(crabber::voice::Recorder, Instant)>,
    /// Voice message playing: its URL and the player.
    pub player: Option<(String, crabber::voice::Player)>,
    /// Voice message lengths in seconds, read once per file.
    pub audio_len: HashMap<PathBuf, f32>,
    /// Right-click menu over the image viewer.
    pub viewer_menu: Option<Point<Pixels>>,
    /// A 100 ms redraw loop runs (playback progress, recording timer).
    animating: bool,
    /// Last window geometry, to restore it when re-opened from the tray.
    window_bounds: Option<WindowBounds>,
    tray: Option<desktop::Tray>,
    desktop_tx: UnboundedSender<DesktopEvent>,
    /// Picture sizes for the chat bubbles: reading the file header every frame made scrolling stutter.
    pub img_dims: HashMap<PathBuf, (f32, f32)>,
    app_store: Store,
    _subs: Vec<Subscription>,
}

/// Keeps the App entity (and the XMPP session in it) alive while the window is closed to the tray.
struct AppGlobal(Entity<App>);

impl Global for AppGlobal {}

/// Opens the main window, re-creating it with the last geometry after hide-to-tray.
pub fn open_main_window(cx: &mut gpui_kit::App) -> gpui_kit::Result<()> {
    let existing = cx.try_global::<AppGlobal>().map(|g| g.0.clone());
    let bounds = existing
        .as_ref()
        .and_then(|app| app.read_with(cx, |app, _| app.window_bounds))
        .unwrap_or_else(|| WindowBounds::centered(size(px(1100.), px(720.)), cx));
    // Launched by autostart: the first window goes to the tray. On Windows it is created hidden (no flash);
    // on Linux a never-shown window may not come back on activate, so it opens and is closed right away.
    let start_hidden = existing.is_none() && std::env::args().any(|a| a == desktop::MINIMIZED_ARG);
    let created_hidden = start_hidden && cfg!(windows);
    let options = WindowOptions {
        window_bounds: Some(bounds),
        show: !created_hidden,
        focus: !created_hidden,
        // Windows: our own title bar (view.rs `renderTitlebar`) replaces the system caption.
        // Elsewhere the window manager's title bar stays (move/resize/buttons work natively).
        titlebar: Some(TitlebarOptions { title: Some("Crabber".into()), appears_transparent: cfg!(windows), traffic_light_position: None }),
        window_min_size: Some(size(px(720.), px(480.))),
        // Wayland: matches crabber.desktop (icon) and is required for activation requests.
        app_id: Some("crabber".into()),
        ..Default::default()
    };
    gpui_kit::open_window(options, cx, |window, cx| {
        let app = existing.unwrap_or_else(|| {
            let app = cx.new(|cx| App::new(window, cx));
            cx.set_global(AppGlobal(app.clone()));
            app
        });
        app.update(cx, |app, cx| app.bind_window(window, start_hidden, cx));
        app
    })
    .map(|_| ())
}

impl App {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let app_store = Store::open(&store::data_dir().join("app.db")).expect("cannot open settings database");
        let saved_jid = app_store.get("jid").unwrap_or_default();
        let saved_host = app_store.get("host").unwrap_or_default();
        let insecure = app_store.get("insecure").is_some_and(|v| v == "1");

        let login_jid = cx.new(|cx| InputState::new(window, cx).placeholder("user@example.com").default_value(saved_jid.clone()));
        let login_pass = cx.new(|cx| InputState::new(window, cx).placeholder("Пароль").masked(true));
        let login_host = cx.new(|cx| InputState::new(window, cx).placeholder("Сервер host[:порт] — необязательно").default_value(saved_host));
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Поиск"));
        // Enter sends, Shift+Enter starts a new line (for ``` code blocks).
        let compose = cx.new(|cx| TextareaState::new(window, cx).placeholder("Написать сообщение…").auto_grow(1, 8).submit_on_enter(true));
        let field1 = cx.new(|cx| InputState::new(window, cx));
        let field2 = cx.new(|cx| InputState::new(window, cx));
        let field3 = cx.new(|cx| TextareaState::new(window, cx).placeholder("-----BEGIN PGP ... KEY BLOCK-----").rows(8));

        let messages = ListState::new(0, ListAlignment::Top, px(600.));
        messages.set_follow_mode(FollowMode::Tail);

        let (tray, desktop_tx, mut desktop_events) = desktop::init(&store::data_dir());
        cx.spawn(async move |this, cx| {
            while let Some(ev) = desktop_events.next().await {
                if matches!(ev, DesktopEvent::Quit) {
                    let _ = this.update(cx, |_, cx| cx.quit());
                    break;
                }
                // On Wayland the window is closed while in the tray: re-create it before delivering.
                // Not inside an entity update: open_main_window updates the App entity itself.
                let mut deliver = true;
                let opened = cx.update(|cx| {
                    if !cx.windows().is_empty() {
                        return true;
                    }
                    match open_main_window(cx) {
                        // A fresh window is already the shown state: a Toggle is complete.
                        Ok(()) => deliver = !matches!(ev, DesktopEvent::Toggle),
                        Err(_) => return false,
                    }
                    true
                });
                if !opened {
                    break;
                }
                if deliver && this.update_in(cx, |app, window, cx| app.on_desktop(ev, window, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();

        let mut app = Self {
            login_jid,
            login_pass,
            login_host,
            insecure,
            remember: true,
            login_error: "".into(),
            connecting: false,
            sess: None,
            search,
            compose,
            field1,
            field2,
            field3,
            modal: None,
            messages,
            viewer: None,
            viewer_focus: cx.focus_handle(),
            settings: Settings::load(&app_store),
            search_results: vec![],
            search_all: false,
            root_focus: cx.focus_handle(),
            logo: Arc::new(Image::from_bytes(ImageFormat::Png, desktop::ICON_PNG.to_vec())),
            msg_menu: None,
            emoji_picker: None,
            recording: None,
            player: None,
            audio_len: HashMap::new(),
            viewer_menu: None,
            animating: false,
            window_bounds: None,
            tray,
            desktop_tx,
            img_dims: HashMap::new(),
            app_store,
            _subs: vec![],
        };
        app.apply_theme(window, cx);
        // Older versions registered autostart without the tray flag: rewrite the entry.
        if app.settings.autostart {
            desktop::set_autostart(true);
        }
        // Auto-login with a remembered password.
        if !saved_jid.is_empty()
            && let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &saved_jid)
            && let Ok(password) = entry.get_password()
        {
            app.start_session(saved_jid, password, cx);
        }
        app
    }

    /// Window-scoped setup, re-run every time the window is (re)created after hide-to-tray.
    fn bind_window(&mut self, window: &mut Window, start_hidden: bool, cx: &mut Context<Self>) {
        desktop::set_window(window);
        self._subs = vec![
            cx.observe_window_bounds(window, |this, window, _| {
                this.window_bounds = Some(window.window_bounds());
            }),
            cx.subscribe_in(&self.login_pass, window, |this, _, ev: &InputEvent, _, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.login(cx);
                }
            }),
            cx.subscribe_in(&self.compose, window, |this, _, ev: &InputEvent, window, cx| match ev {
                InputEvent::PressEnter { shift: false, .. } => this.send_text(window, cx),
                InputEvent::Change => {
                    this.on_compose_change(cx);
                    cx.notify(); // the send button shows only with text
                }
                _ => {}
            }),
            cx.subscribe_in(&self.search, window, |this, _, ev: &InputEvent, _, cx| {
                if let InputEvent::Change = ev {
                    this.run_search(cx);
                }
            }),
            cx.observe_window_appearance(window, |this, window, cx| {
                if this.settings.theme == "system" {
                    Theme::sync_system_appearance(Some(window), cx);
                    crate::style::apply(cx, this.settings.ui_scale);
                }
            }),
            cx.subscribe_in(&self.field2, window, |this, _, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.submit_modal(window, cx);
                }
            }),
            cx.observe_window_activation(window, |this, window, cx| {
                if window.is_window_active() {
                    this.mark_read(cx);
                }
            }),
        ];
        // Closing the window keeps the app in the tray while logged in.
        let weak = cx.weak_entity();
        let has_tray = self.tray.is_some();
        window.on_window_should_close(cx, move |window, cx| {
            let logged_in = weak.read_with(cx, |app, _| app.sess.is_some()).unwrap_or(false);
            if logged_in && has_tray {
                desktop::hide_window(window);
                false
            } else {
                cx.quit();
                true
            }
        });
        self.root_focus.focus(window, cx);
        if start_hidden {
            // Stay in the tray only if it can bring the window back and we log in by ourselves;
            // otherwise the login screen must be seen.
            let keep_hidden = self.tray.is_some() && self.sess.is_some();
            cx.defer_in(window, move |_, window, _| if keep_hidden { desktop::hide_window(window) } else { desktop::show_window(window) });
        }
    }

    pub fn login(&mut self, cx: &mut Context<Self>) {
        if self.connecting || self.sess.is_some() {
            return;
        }
        let jid = self.login_jid.read(cx).value().trim().to_string();
        let password = self.login_pass.read(cx).value().to_string();
        if BareJid::from_str(&jid).is_err() || !jid.contains('@') {
            self.login_error = "Неверный JID, нужен формат user@server".into();
            cx.notify();
            return;
        }
        if password.is_empty() {
            self.login_error = "Введите пароль".into();
            cx.notify();
            return;
        }
        self.start_session(jid, password, cx);
    }

    fn start_session(&mut self, jid: String, password: String, cx: &mut Context<Self>) {
        let host_field = self.login_host.read(cx).value().trim().to_string();
        let (host, port) = match host_field.rsplit_once(':') {
            Some((h, p)) if p.parse::<u16>().is_ok() => (Some(h.to_string()), p.parse().unwrap()),
            _ if !host_field.is_empty() => (Some(host_field.clone()), 5222),
            _ => (None, 5222),
        };
        let dir = store::account_dir(&jid);
        let store = match Store::open(&dir.join("chat.db")) {
            Ok(s) => s,
            Err(e) => {
                self.login_error = format!("Не удалось открыть базу: {e}").into();
                cx.notify();
                return;
            }
        };
        self.app_store.set("jid", &jid);
        self.app_store.set("host", &host_field);
        self.app_store.set("insecure", if self.insecure { "1" } else { "0" });
        if self.remember
            && let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &jid)
        {
            let _ = entry.set_password(&password);
        }

        let (cmd, mut events) = xmpp::connect(Account {
            jid: BareJid::from_str(&jid).unwrap(),
            password,
            host,
            port,
            insecure: self.insecure,
            dir: dir.clone(),
        });
        let _ = cmd.unbounded_send(Cmd::E2eBlindTrust(self.settings.blind_trust));
        let mut chats = store.chats();
        chats.sort_by_key(|c| std::cmp::Reverse(c.last.as_ref().map_or(0, |m| m.ts)));
        let mut sess = Session {
            me: jid.clone(),
            cmd,
            store,
            dir,
            online: false,
            ever_online: false,
            status: "Подключение...".into(),
            show: None,
            chats,
            presence: HashSet::new(),
            typing: HashMap::new(),
            occupants: HashMap::new(),
            requests: vec![],
            avatars: HashMap::new(),
            selected: None,
            history: vec![],
            downloads: HashSet::new(),
            failed: HashSet::new(),
            displayed_sent: HashMap::new(),
            typing_sent: None,
            call: None,
            pending: vec![],
            uploads: HashMap::new(),
            flash: None,
            e2e_devices: HashMap::new(),
            pgp_own: None,
            pgp_contacts: HashMap::new(),
            editing: None,
            reactions: HashMap::new(),
            replying: None,
            mds: HashMap::new(),
            blocked: None,
            members: HashMap::new(),
        };
        for peer in sess.chats.iter().map(|c| c.peer.clone()).chain([jid]).collect::<Vec<_>>() {
            sess.load_avatar(&peer);
        }
        self.sess = Some(sess);
        self.connecting = true;
        self.login_error = "".into();

        cx.spawn(async move |this, cx| {
            while let Some(first) = events.next().await {
                let mut batch = vec![first];
                while let Ok(ev) = events.try_recv() {
                    batch.push(ev);
                }
                // The window may be closed to the tray (Wayland): events keep processing without it.
                let windowed = this.update(cx, |_, cx| !cx.windows().is_empty());
                let ok = match windowed {
                    Ok(true) => this.update_in(cx, |app, window, cx| app.on_events(batch, Some(window), cx)).is_ok(),
                    Ok(false) => this.update(cx, |app, cx| app.on_events(batch, None, cx)).is_ok(),
                    Err(_) => false,
                };
                if !ok {
                    break;
                }
            }
        })
        .detach();
        // Ticks the call timer and expires stale "typing..." indicators.
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
                let alive = this.update(cx, |app, cx| {
                    let Some(s) = app.sess.as_mut() else { return false };
                    s.typing.retain(|_, (_, at)| at.elapsed().as_secs() < 30);
                    if s.flash.as_ref().is_some_and(|(_, at)| at.elapsed().as_secs() >= 3) {
                        s.flash = None;
                        cx.notify();
                    }
                    if s.call.is_some() || !s.typing.is_empty() || s.flash.is_some() {
                        cx.notify();
                    }
                    true
                });
                if !matches!(alive, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    pub fn logout(&mut self, cx: &mut Context<Self>) {
        if let Some(sess) = self.sess.take()
            && let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &sess.me)
        {
            let _ = entry.delete_credential();
        }
        self.connecting = false;
        self.modal = None;
        self.messages.reset(0);
        cx.notify();
    }

    pub fn apply_theme(&self, window: &mut Window, cx: &mut Context<Self>) {
        match self.settings.theme.as_str() {
            "light" => Theme::change(ThemeMode::Light, Some(window), cx),
            "dark" => Theme::change(ThemeMode::Dark, Some(window), cx),
            _ => Theme::sync_system_appearance(Some(window), cx),
        }
        crate::style::apply(cx, self.settings.ui_scale);
    }

    pub const SCALES: [f32; 7] = [0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75];

    /// Ctrl+= / Ctrl+-: next/previous step of `SCALES`.
    pub fn step_scale(&mut self, up: bool, window: &mut Window, cx: &mut Context<Self>) {
        let cur = self.settings.ui_scale;
        let next = if up {
            Self::SCALES.iter().copied().find(|s| *s > cur + 0.01)
        } else {
            Self::SCALES.iter().rev().copied().find(|s| *s < cur - 0.01)
        };
        if let Some(scale) = next {
            self.update_settings(window, cx, |st| st.ui_scale = scale);
        }
    }

    /// Changes a preference, persists it and re-applies the theme.
    pub fn update_settings(&mut self, window: &mut Window, cx: &mut Context<Self>, f: impl FnOnce(&mut Settings)) {
        f(&mut self.settings);
        self.settings.save(&self.app_store);
        self.apply_theme(window, cx);
        cx.notify();
    }

    pub fn run_search(&mut self, cx: &mut Context<Self>) {
        let query = self.search.read(cx).value().trim().to_string();
        let all = self.search_all;
        self.search_results = match self.sess.as_ref() {
            Some(sess) if query.chars().count() >= 2 => {
                let scope = sess.selected.as_deref().filter(|_| !all);
                sess.store.search_in(scope, &query, 100)
            }
            _ => vec![],
        };
        cx.notify();
    }

    pub fn set_search_all(&mut self, all: bool, cx: &mut Context<Self>) {
        self.search_all = all;
        self.run_search(cx);
    }

    /// Esc, innermost first: viewer, dialog, search, open chat. Returns whether it did anything.
    pub fn handle_escape(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.msg_menu.take().is_some() || self.emoji_picker.take().is_some() || self.viewer_menu.take().is_some() {
        } else if self.recording.take().is_some() {
            // Esc throws the recording away.
        } else if self.viewer.take().is_some() {
            // Back to whatever was under the viewer (e.g. the media panel).
        } else if self.modal.take().is_some() {
        } else if let Some(sess) = self.sess.as_mut().filter(|s| s.editing.is_some()) {
            sess.editing = None;
            self.compose.update(cx, |s, cx| s.set_value("", window, cx));
        } else if let Some(sess) = self.sess.as_mut().filter(|s| s.replying.is_some()) {
            sess.replying = None;
        } else if !self.search.read(cx).value().is_empty() {
            self.search.update(cx, |s, cx| s.set_value("", window, cx));
            self.run_search(cx);
        } else if let Some(sess) = self.sess.as_mut().filter(|s| s.selected.is_some()) {
            sess.selected = None;
            sess.history.clear();
            sess.pending.clear();
            self.messages.reset(0);
            self.search_all = false;
        } else {
            return false;
        }
        self.root_focus.focus(window, cx);
        cx.notify();
        true
    }

    /// Opens a chat scrolled to a message and highlights it.
    pub fn open_message(&mut self, peer: String, msg_id: String, ts: i64, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sess) = self.sess.as_ref() else { return };
        let limit = (sess.store.count_since(&peer, ts) + 50).max(500);
        self.select_with(peer, limit, window, cx);
        let Some(sess) = self.sess.as_mut() else { return };
        if let Some(ix) = sess.history.iter().position(|m| m.msg_id == msg_id) {
            // Tail-follow would snap back to the newest message.
            self.messages.set_follow_mode(FollowMode::Normal);
            self.messages.scroll_to(ListOffset { item_ix: ix.saturating_sub(2), offset_in_item: px(0.) });
        }
        sess.flash = Some((msg_id, Instant::now()));
        cx.notify();
    }

    pub fn open_media(&mut self, peer: String, cx: &mut Context<Self>) {
        let Some(sess) = self.sess.as_ref() else { return };
        let mut items = sess.store.attachments(&peer);
        items.reverse();
        let links = sess.store.link_messages(&peer);
        let tab = if items.iter().any(|m| m.url.as_deref().is_some_and(is_image)) { MediaTab::Photos } else { MediaTab::Files };
        self.modal = Some(Modal::Media { peer, tab, items, links });
        cx.notify();
    }

    pub fn open_encryption(&mut self, peer: String, cx: &mut Context<Self>) {
        if let Some(sess) = &self.sess {
            sess.send(Cmd::E2eInfo { jid: peer.clone() });
            sess.send(Cmd::PgpInfo { jid: Some(peer.clone()) });
        }
        self.modal = Some(Modal::Encryption { peer });
        cx.notify();
    }

    pub fn open_settings(&mut self, cx: &mut Context<Self>) {
        if let Some(sess) = &self.sess {
            sess.send(Cmd::E2eInfo { jid: sess.me.clone() });
            sess.send(Cmd::PgpInfo { jid: None });
        }
        self.modal = Some(Modal::Settings);
        cx.notify();
    }

    /// "auto" | "omemo" | "plain" (| "pgp")
    pub fn set_chat_enc(&mut self, peer: &str, mode: &str, cx: &mut Context<Self>) {
        let Some(sess) = self.sess.as_mut() else { return };
        if let Some(c) = sess.chat_mut(peer) {
            c.enc = mode.to_string();
        }
        sess.save(peer);
        cx.notify();
    }

    pub fn pgp_cmd(&mut self, cmd: Cmd) {
        if let Some(sess) = &self.sess {
            sess.send(cmd);
        }
    }

    pub fn open_pgp_import(&mut self, jid: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.field3.update(cx, |s, cx| {
            s.set_value("", window, cx);
            s.focus(window, cx);
        });
        self.modal = Some(Modal::PgpImport { jid });
        cx.notify();
    }

    pub fn submit_pgp_import(&mut self, cx: &mut Context<Self>) {
        let armored = self.field3.read(cx).value().to_string();
        let Some(Modal::PgpImport { jid }) = self.modal.take() else { return };
        let Some(sess) = &self.sess else { return };
        match jid {
            Some(jid) => {
                sess.send(Cmd::PgpImportContact { jid: jid.clone(), armored });
                self.modal = Some(Modal::Encryption { peer: jid });
            }
            None => {
                sess.send(Cmd::PgpImportSecret { armored });
                self.modal = Some(Modal::Settings);
            }
        }
        cx.notify();
    }

    pub fn set_trust(&mut self, jid: String, device: u32, trusted: bool) {
        if let Some(sess) = &self.sess {
            sess.send(Cmd::E2eTrust { jid, device, trusted });
        }
    }

    pub fn set_media_tab(&mut self, new_tab: MediaTab, cx: &mut Context<Self>) {
        if let Some(Modal::Media { tab, .. }) = self.modal.as_mut() {
            *tab = new_tab;
            cx.notify();
        }
    }

    /// Moves the image viewer to the previous/next picture of the chat.
    pub fn viewer_step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(Viewer::Image { url, gallery, .. }) = self.viewer.as_ref() else { return };
        let Some(ix) = gallery.iter().position(|u| u == url) else { return };
        let next = ix as isize + delta;
        if next < 0 || next as usize >= gallery.len() {
            return;
        }
        let (url, gallery) = (gallery[next as usize].clone(), gallery.clone());
        let Some(sess) = self.sess.as_ref() else { return };
        let path = sess.store.file_for(&url).unwrap_or_default();
        if !path.is_file() {
            self.request_download(&url);
        }
        self.viewer = Some(Viewer::Image { path, url, gallery });
        cx.notify();
    }

    fn on_events(&mut self, batch: Vec<Ev>, mut window: Option<&mut Window>, cx: &mut Context<Self>) {
        let (notify, notify_text) = (self.settings.notify, self.settings.notify_text);
        let Some(sess) = self.sess.as_mut() else { return };
        sess.store.begin();
        let mut notes = vec![];
        let mut failed_login = None;
        let mut call_log = vec![];
        let active = window.as_ref().is_some_and(|w| w.is_window_active());
        for ev in batch {
            match ev {
                Ev::Online { .. } => {
                    sess.online = true;
                    sess.ever_online = true;
                    sess.status = "В сети".into();
                    let rooms = sess
                        .chats
                        .iter()
                        .filter(|c| c.is_room)
                        .map(|c| (c.peer.clone(), c.nick.clone(), sess.store.last_ts(&c.peer)))
                        .collect();
                    sess.send(Cmd::Sync { mam_after: sess.store.get("mam_last"), rooms });
                    if sess.show.is_some() {
                        sess.send(Cmd::SetShow(sess.show.clone()));
                    }
                }
                Ev::Disconnected(e) => {
                    if !sess.ever_online {
                        failed_login = Some(e);
                        break;
                    }
                    sess.online = false;
                    sess.presence.clear();
                    sess.members.clear();
                    sess.status = format!("Нет соединения, переподключение... ({e})").into();
                }
                Ev::Roster(contacts) => {
                    let jids: HashSet<String> = contacts.iter().map(|c| c.jid.clone()).collect();
                    for c in sess.chats.iter_mut().filter(|c| !c.is_room && c.in_roster && !jids.contains(&c.peer)) {
                        c.in_roster = false;
                        sess.store.save_chat(c);
                    }
                    for c in contacts {
                        apply_contact(sess, c);
                    }
                }
                Ev::RosterItem(c) => apply_contact(sess, c),
                Ev::Presence { jid, online } => {
                    if online {
                        sess.presence.insert(jid);
                    } else {
                        sess.presence.remove(&jid);
                    }
                }
                Ev::SubscribeRequest { jid } => {
                    // We asked for their presence already (we added them): approve automatically.
                    let ours = sess.chat(&jid).is_some_and(|c| c.in_roster && (c.subscription == "to" || c.subscription == "none"));
                    if ours {
                        sess.send(Cmd::Approve { jid });
                    } else if !sess.requests.contains(&jid) {
                        sess.requests.push(jid);
                    }
                }
                Ev::Message { peer, groupchat, mut msg, archive_id, live, sid } => {
                    if let Some(id) = archive_id {
                        sess.store.set("mam_last", &id);
                    }
                    if let Some(r) = msg.reply_to.take() {
                        msg.reply_to = Some(sess.store.resolve(&peer, &r).unwrap_or(r));
                    }
                    let inserted = sess.store.insert_msg(&peer, &msg);
                    // Our own groupchat messages learn their stanza-id from the room's echo.
                    if let Some(sid) = &sid {
                        sess.store.set_sid(&peer, &msg.msg_id, sid);
                    }
                    if !inserted {
                        // Duplicate: MUC echo / carbon of something we sent confirms delivery.
                        if msg.outgoing && sess.store.set_status(&peer, &msg.msg_id, 1) {
                            update_history(sess, &peer, |m| {
                                if m.msg_id == msg.msg_id && m.status < 1 {
                                    m.status = 1;
                                }
                            });
                        }
                        continue;
                    }
                    let viewing = sess.selected.as_deref() == Some(&peer) && active;
                    // Already read on another device (XEP-0490)?
                    let read_ts = read_marker_ts(sess, &peer);
                    let seen = read_ts.is_some_and(|t| msg.ts <= t);
                    if notify && live && !msg.outgoing && !viewing && !seen {
                        let title = if groupchat { format!("{} — {}", sess.display_name(&peer), msg.sender) } else { sess.display_name(&peer) };
                        let body = match &msg.url {
                            Some(u) if is_image(u) => "Фото".to_string(),
                            Some(u) if is_audio(u) => "Голосовое сообщение".to_string(),
                            Some(_) => "Файл".to_string(),
                            None if notify_text => msg.body.clone(),
                            None => "Новое сообщение".to_string(),
                        };
                        desktop::notify(&peer, &title, &body, Some(&store::avatar_path(&sess.dir, &peer)));
                    }
                    let marker = sid.is_some() && sess.mds.get(&peer) == sid.as_ref();
                    // The marked message itself arrived: recount, earlier ones were counted as unread.
                    let recount = marker.then(|| sess.store.incoming_after(&peer, msg.ts));
                    let chat = sess.ensure_chat(&peer, groupchat);
                    if let Some(n) = recount {
                        chat.unread = n;
                    } else if !msg.outgoing && !viewing && !seen {
                        chat.unread += 1;
                    }
                    if chat.last.as_ref().is_none_or(|l| l.ts <= msg.ts) {
                        chat.last = Some(msg.clone());
                    }
                    if msg.outgoing && chat.unread > 0 && !groupchat {
                        chat.unread = 0; // we answered from another device
                    }
                    sess.save(&peer);
                    sess.typing.remove(&peer);
                    if sess.selected.as_deref() == Some(&peer) {
                        // Archive pages can be older than what is shown: insert in time order.
                        let pos = sess.history.iter().rposition(|m| m.ts <= msg.ts).map_or(0, |p| p + 1);
                        sess.history.insert(pos, msg);
                        self.messages.splice(pos..pos, 1);
                    }
                }
                Ev::Correct { peer, id, sender, body } => apply_correction(sess, &peer, &id, &sender, &body),
                Ev::Mds { peer, sid } => {
                    sess.mds.insert(peer.clone(), sid);
                    if let Some(ts) = read_marker_ts(sess, &peer) {
                        let n = sess.store.incoming_after(&peer, ts);
                        if let Some(c) = sess.chat_mut(&peer)
                            && c.unread != n
                        {
                            c.unread = n;
                            sess.save(&peer);
                        }
                    }
                }
                Ev::Blocklist(list) => sess.blocked = Some(list.into_iter().collect()),
                Ev::Blocked { jid, on } => {
                    let set = sess.blocked.get_or_insert_default();
                    match jid {
                        Some(j) if on => {
                            set.insert(j);
                        }
                        Some(j) => {
                            set.remove(&j);
                        }
                        None => set.clear(),
                    }
                }
                Ev::Reactions { peer, id, sender, emojis } => {
                    if let Some(msg_id) = sess.store.resolve(&peer, &id) {
                        sess.store.set_reactions(&peer, &msg_id, &sender, &emojis);
                        if sess.selected.as_deref() == Some(&peer) {
                            sess.reactions = sess.store.reactions(&peer);
                        }
                    }
                }
                Ev::Bookmarks(list) => {
                    // After the first sync a room missing on the server was left on another device.
                    let synced = sess.store.get("bookmarks_synced").is_some() && list.is_some();
                    let list = list.unwrap_or_default();
                    let remote: HashSet<String> = list.iter().map(|b| b.room.clone()).collect();
                    let missing: Vec<String> = sess.chats.iter().filter(|c| c.is_room && !remote.contains(&c.peer)).map(|c| c.peer.clone()).collect();
                    for room in missing {
                        if synced {
                            forget_room(sess, &mut self.messages, &room);
                        } else {
                            sess.publish_bookmark(&room);
                        }
                    }
                    for b in list {
                        add_bookmark(sess, b);
                    }
                    sess.store.set("bookmarks_synced", "1");
                }
                Ev::BookmarkAdded(b) => add_bookmark(sess, b),
                Ev::BookmarkRemoved { room } => {
                    if sess.chat(&room).is_some_and(|c| c.is_room) {
                        forget_room(sess, &mut self.messages, &room);
                    }
                }
                Ev::Receipt { peer, id } => {
                    if sess.store.set_status(&peer, &id, 1) {
                        update_history(sess, &peer, |m| {
                            if m.msg_id == id && m.status < 1 {
                                m.status = 1;
                            }
                        });
                    }
                }
                Ev::Displayed { peer, id, by_me } => {
                    if by_me {
                        if let Some(c) = sess.chat_mut(&peer) {
                            c.unread = 0;
                        }
                        sess.save(&peer);
                    } else {
                        sess.store.mark_read_until(&peer, &id);
                        let ts = sess.history.iter().find(|m| m.msg_id == id).map(|m| m.ts);
                        if let Some(ts) = ts {
                            update_history(sess, &peer, |m| {
                                if m.outgoing && m.ts <= ts && m.status >= 0 {
                                    m.status = 2;
                                }
                            });
                        }
                    }
                }
                Ev::Typing { peer, who, on } => {
                    if on {
                        sess.typing.insert(peer, (who, Instant::now()));
                    } else {
                        sess.typing.remove(&peer);
                    }
                }
                Ev::RoomJoined { room, occupants } => {
                    sess.ensure_chat(&room, true);
                    sess.occupants.insert(room, occupants);
                }
                Ev::RoomName { room, name } => {
                    sess.ensure_chat(&room, true).name = name;
                    sess.save(&room);
                }
                Ev::RoomError { room, error } => notes.push(format!("Группа {room}: {error}")),
                Ev::Member { room, occupant, gone } => {
                    let list = sess.members.entry(room).or_default();
                    if gone {
                        list.remove(&occupant.nick);
                    } else {
                        list.insert(occupant.nick.clone(), occupant);
                    }
                }
                Ev::RoomRemoved { room, reason } => {
                    sess.members.remove(&room);
                    sess.occupants.remove(&room);
                    notes.push(format!("{}: {reason}", sess.display_name(&room)));
                }
                Ev::RoomLost { room } => {
                    if let Some(c) = sess.chat(&room) {
                        let nick = c.nick.clone();
                        sess.send(Cmd::Join { room: room.clone(), nick, since: sess.store.last_ts(&room), create_name: None });
                    }
                }
                Ev::Retract { peer, id, sender } => {
                    if let Some(msg_id) = sess.store.resolve(&peer, &id) {
                        apply_retract(sess, &peer, &msg_id, sender.as_deref());
                    }
                }
                Ev::Invite { room, from } => {
                    if sess.chat(&room).is_none() {
                        let nick = sess.localpart();
                        sess.ensure_chat(&room, true);
                        sess.send(Cmd::Join { room: room.clone(), nick, since: None, create_name: None });
                        sess.publish_bookmark(&room);
                        notes.push(format!("{from} добавил вас в группу {room}"));
                    }
                }
                Ev::Avatar { jid } => sess.load_avatar(&jid),
                Ev::Uploaded { peer, id, url } => {
                    sess.store.set_url(&peer, &id, &url);
                    if let Some(src) = sess.uploads.remove(&id) {
                        sess.store.set_file(&url, &src);
                    }
                    update_history(sess, &peer, |m| {
                        if m.msg_id == id {
                            m.url = Some(url.clone());
                            m.body = url.clone();
                        }
                    });
                }
                Ev::SendFailed { peer, id, error } => {
                    if !sess.store.set_failed(&peer, &id) {
                        continue;
                    }
                    update_history(sess, &peer, |m| {
                        if m.msg_id == id {
                            m.status = -1;
                        }
                    });
                    notes.push(format!("Не отправлено: {error}"));
                }
                Ev::SentEncrypted { peer, id, enc } => {
                    sess.store.set_enc(&peer, &id, enc);
                    update_history(sess, &peer, |m| {
                        if m.msg_id == id {
                            m.enc = enc;
                        }
                    });
                }
                Ev::E2eDevices { jid, devices } => {
                    sess.e2e_devices.insert(jid, devices);
                }
                Ev::PgpOwn { fingerprint, public } => {
                    sess.pgp_own = fingerprint.zip(public);
                }
                Ev::PgpContact { jid, fingerprint, hint } => {
                    sess.pgp_contacts.insert(jid, (fingerprint, hint));
                }
                Ev::Downloaded { url } => {
                    sess.downloads.remove(&url);
                }
                Ev::DownloadFailed { url, error } => {
                    sess.downloads.remove(&url);
                    sess.failed.insert(url);
                    eprintln!("download failed: {error}");
                }
                Ev::Error(e) => notes.push(e),
                Ev::Call(CallEv::State { peer, state }) => {
                    let ended = matches!(state, CallState::Ended(_));
                    if let CallState::Ended(reason) = &state
                        && !reason.is_empty()
                    {
                        notes.push(format!("Звонок завершён: {reason}"));
                    }
                    // Ringing in a minimized or tray-hidden window would go unseen: bring the window up.
                    if state == CallState::Incoming && sess.call.is_none() {
                        let _ = self.desktop_tx.unbounded_send(DesktopEvent::Show);
                    }
                    if ended {
                        call_log.extend(sess.call.take());
                    } else {
                        let call = sess.call.get_or_insert(CallUi { peer: peer.clone(), state: state.clone(), muted: false, started: None, outgoing: state == CallState::Outgoing });
                        if state == CallState::Active && call.started.is_none() {
                            call.started = Some(Instant::now());
                        }
                        call.peer = peer;
                        call.state = state;
                    }
                }
            }
        }
        sess.store.commit();
        if let Some(e) = failed_login {
            self.sess = None;
            self.connecting = false;
            self.login_error = format!("Не удалось войти: {e}").into();
            cx.notify();
            return;
        }
        sess.sort_chats();
        for c in call_log {
            self.log_call(c);
        }
        self.connecting = false;
        if let Some(window) = window.as_deref_mut() {
            for n in notes {
                window.push_notification(n, cx);
            }
            self.update_title(Some(window));
        } else {
            self.update_title(None);
        }
        self.mark_read_in(active, cx);
        cx.notify();
    }

    fn update_title(&mut self, window: Option<&mut Window>) {
        let unread: u32 = self.sess.as_ref().map_or(0, |s| s.chats.iter().map(|c| c.unread).sum());
        if let Some(tray) = self.tray.as_mut() {
            tray.set_unread(unread);
        }
        if let Some(window) = window {
            window.set_window_title(&if unread > 0 { format!("Crabber ({unread})") } else { "Crabber".to_string() });
        }
    }

    pub fn select(&mut self, peer: String, window: &mut Window, cx: &mut Context<Self>) {
        self.select_with(peer, 500, window, cx);
    }

    fn select_with(&mut self, peer: String, limit: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sess) = self.sess.as_mut() else { return };
        sess.history = sess.store.history(&peer, limit);
        sess.reactions = sess.store.reactions(&peer);
        sess.editing = None;
        sess.replying = None;
        self.messages.set_follow_mode(FollowMode::Tail);
        sess.selected = Some(peer);
        sess.typing_sent = None;
        self.messages.reset(sess.history.len());
        self.compose.update(cx, |s, cx| {
            s.set_value("", window, cx);
            s.focus(window, cx);
        });
        self.mark_read_in(window.is_window_active(), cx);
        self.update_title(Some(window));
        self.run_search(cx);
        cx.notify();
    }

    fn mark_read(&mut self, cx: &mut Context<Self>) {
        self.mark_read_in(true, cx);
    }

    fn mark_read_in(&mut self, active: bool, cx: &mut Context<Self>) {
        let Some(sess) = self.sess.as_mut() else { return };
        let Some(peer) = sess.selected.clone() else { return };
        if active {
            read_chat(sess, &peer);
            cx.notify();
        }
    }

    fn on_compose_change(&mut self, cx: &mut Context<Self>) {
        let empty = self.compose.read(cx).value().trim().is_empty();
        let Some(sess) = self.sess.as_mut() else { return };
        let Some(peer) = sess.selected.clone() else { return };
        if sess.chat(&peer).is_some_and(|c| c.is_room) {
            return;
        }
        match (empty, sess.typing_sent) {
            (false, None) => {
                sess.typing_sent = Some(Instant::now());
                sess.send(Cmd::Typing { to: peer, on: true });
            }
            (false, Some(t)) if t.elapsed().as_secs() > 10 => {
                sess.typing_sent = Some(Instant::now());
                sess.send(Cmd::Typing { to: peer, on: true });
            }
            (true, Some(_)) => {
                sess.typing_sent = None;
                sess.send(Cmd::Typing { to: peer, on: false });
            }
            _ => {}
        }
    }

    fn push_outgoing(&mut self, peer: &str, msg: Msg) {
        let Some(sess) = self.sess.as_mut() else { return };
        sess.store.insert_msg(peer, &msg);
        let chat = sess.ensure_chat(peer, false);
        chat.last = Some(msg.clone());
        sess.save(peer);
        if sess.selected.as_deref() == Some(peer) {
            sess.history.push(msg);
            let n = sess.history.len();
            self.messages.splice(n - 1..n - 1, 1);
        }
        sess.sort_chats();
    }

    pub fn send_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let body = self.compose.read(cx).value().trim_end().to_string();
        let Some(sess) = self.sess.as_mut() else { return };
        let Some(peer) = sess.selected.clone() else { return };
        let editing = sess.editing.take();
        let replying = sess.replying.take();
        for path in std::mem::take(&mut sess.pending) {
            self.send_file(&peer, path);
        }
        match editing {
            Some(id) => self.send_correction(&peer, id, body),
            None if !body.trim().is_empty() => self.send_body(&peer, body, replying),
            None => {}
        }
        self.compose.update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    fn send_body(&mut self, peer: &str, body: String, replying: Option<Msg>) {
        let Some(sess) = self.sess.as_mut() else { return };
        let peer = peer.to_string();
        let groupchat = sess.chat(&peer).is_some_and(|c| c.is_room);
        let sender = if groupchat { sess.chat(&peer).map(|c| c.nick.clone()).unwrap_or_default() } else { sess.me.clone() };
        let id = xmpp::new_id();
        sess.typing_sent = None;
        let enc = sess.chat(&peer).map(|c| c.enc.clone()).unwrap_or_default();
        let reply = replying.as_ref().map(|r| reply_for(sess, &peer, groupchat, r));
        sess.send(Cmd::Send { to: peer.clone(), id: id.clone(), body: body.clone(), groupchat, enc, reply });
        let reply_to = replying.map(|r| r.msg_id);
        self.push_outgoing(&peer, Msg { msg_id: id, sender, outgoing: true, body, url: None, ts: now_ms(), status: 0, enc: 0, edited: false, reply_to, retracted: false });
    }

    /// "Reply" from the message menu.
    pub fn start_reply(&mut self, m: &Msg, window: &mut Window, cx: &mut Context<Self>) {
        self.msg_menu = None;
        let Some(sess) = self.sess.as_mut() else { return };
        if sess.editing.take().is_some() {
            self.compose.update(cx, |s, cx| s.set_value("", window, cx));
        }
        sess.replying = Some(m.clone());
        self.compose.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    pub fn cancel_reply(&mut self, cx: &mut Context<Self>) {
        if let Some(sess) = self.sess.as_mut() {
            sess.replying = None;
        }
        cx.notify();
    }

    /// After the confirmation dialog: delete for everyone (our message, or as a moderator).
    pub fn retract(&mut self, m: &Msg, moderate: bool, cx: &mut Context<Self>) {
        self.modal = None;
        let Some(sess) = self.sess.as_mut() else { return };
        let Some(peer) = sess.selected.clone() else { return };
        let groupchat = sess.chat(&peer).is_some_and(|c| c.is_room);
        let sid = sess.store.sid(&peer, &m.msg_id);
        if moderate {
            // The room's broadcast of the deletion updates us like everyone else.
            if let Some(sid) = sid {
                sess.send(Cmd::Moderate { room: peer, sid });
            }
        } else {
            let id = if groupchat { sid.unwrap_or_else(|| m.msg_id.clone()) } else { m.msg_id.clone() };
            sess.send(Cmd::Retract { to: peer.clone(), id, groupchat });
            apply_retract(sess, &peer, &m.msg_id, None);
        }
        cx.notify();
    }

    /// XEP-0045 role/affiliation change in the open group.
    pub fn muc_admin(&mut self, nick: Option<String>, jid: Option<String>, role: Option<&str>, affiliation: Option<&str>, cx: &mut Context<Self>) {
        let Some(sess) = self.sess.as_ref() else { return };
        let Some(Modal::Profile { peer }) = self.modal.as_ref() else { return };
        sess.send(Cmd::MucAdmin { room: peer.clone(), nick, jid, role: role.map(String::from), affiliation: affiliation.map(String::from) });
        cx.notify();
    }

    /// XEP-0191: (un)block a contact; `spam` also reports it (XEP-0377).
    pub fn block(&mut self, jid: String, block: bool, spam: bool, cx: &mut Context<Self>) {
        let Some(sess) = self.sess.as_mut() else { return };
        sess.requests.retain(|j| *j != jid);
        let set = sess.blocked.get_or_insert_default();
        if block {
            set.insert(jid.clone());
        } else {
            set.remove(&jid);
        }
        sess.send(Cmd::Block { jid, block, spam });
        cx.notify();
    }

    fn send_correction(&mut self, peer: &str, id: String, body: String) {
        let Some(sess) = self.sess.as_mut() else { return };
        let unchanged = sess.history.iter().any(|m| m.msg_id == id && m.body == body);
        if body.trim().is_empty() || unchanged {
            return;
        }
        let groupchat = sess.chat(peer).is_some_and(|c| c.is_room);
        let enc = sess.chat(peer).map(|c| c.enc.clone()).unwrap_or_default();
        let sender = sess.my_sender(peer);
        sess.send(Cmd::Correct { to: peer.to_string(), replace: id.clone(), body: body.clone(), groupchat, enc });
        apply_correction(sess, peer, &id, &sender, &body);
    }

    /// "Edit" from the message menu: loads our text into the composer.
    pub fn start_edit(&mut self, m: &Msg, window: &mut Window, cx: &mut Context<Self>) {
        self.msg_menu = None;
        let Some(sess) = self.sess.as_mut() else { return };
        sess.editing = Some(m.msg_id.clone());
        sess.replying = None;
        let body = m.body.clone();
        self.compose.update(cx, |s, cx| {
            s.set_value(body, window, cx);
            s.focus(window, cx);
        });
        cx.notify();
    }

    pub fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(sess) = self.sess.as_mut() {
            sess.editing = None;
        }
        self.compose.update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    /// Double click: puts our `emoji` on, never takes it off.
    pub fn add_reaction(&mut self, msg_id: &str, emoji: &str, cx: &mut Context<Self>) {
        let Some(sess) = self.sess.as_ref() else { return };
        let me = sess.selected.as_deref().map(|p| sess.my_sender(p)).unwrap_or_default();
        if !sess.reactions.get(msg_id).into_iter().flatten().any(|(s, e)| *s == me && e == emoji) {
            self.toggle_reaction(msg_id, emoji, cx);
        }
    }

    /// Adds or removes our `emoji` on a message of the open chat (XEP-0444).
    pub fn toggle_reaction(&mut self, msg_id: &str, emoji: &str, cx: &mut Context<Self>) {
        self.msg_menu = None;
        self.emoji_picker = None;
        let Some(sess) = self.sess.as_mut() else { return };
        let Some(peer) = sess.selected.clone() else { return };
        let me = sess.my_sender(&peer);
        let mut mine: Vec<String> = sess.reactions.get(msg_id).into_iter().flatten().filter(|(s, _)| *s == me).map(|(_, e)| e.clone()).collect();
        if let Some(ix) = mine.iter().position(|e| e == emoji) {
            mine.remove(ix);
        } else {
            mine.push(emoji.to_string());
        }
        let groupchat = sess.chat(&peer).is_some_and(|c| c.is_room);
        // Groups address messages by the room's stanza-id.
        let id = if groupchat { sess.store.sid(&peer, msg_id).unwrap_or_else(|| msg_id.to_string()) } else { msg_id.to_string() };
        sess.send(Cmd::React { to: peer.clone(), id, emojis: mine.clone(), groupchat });
        sess.store.set_reactions(&peer, msg_id, &me, &mine);
        sess.reactions = sess.store.reactions(&peer);
        cx.notify();
    }

    fn on_desktop(&mut self, ev: DesktopEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            DesktopEvent::Show => desktop::show_window(window),
            DesktopEvent::Toggle => desktop::toggle_window(window),
            DesktopEvent::Quit => cx.quit(),
            DesktopEvent::Open(peer) => {
                desktop::show_window(window);
                if self.sess.as_ref().is_some_and(|s| s.chat(&peer).is_some()) {
                    self.select(peer, window, cx);
                }
            }
            DesktopEvent::Reply(peer, text) => {
                self.send_body(&peer, text.trim().to_string(), None);
                if let Some(sess) = self.sess.as_mut() {
                    read_chat(sess, &peer);
                }
                self.update_title(Some(window));
                cx.notify();
            }
        }
    }

    /// Adds files to the "about to send" panel.
    pub fn add_pending(&mut self, paths: impl IntoIterator<Item = PathBuf>, cx: &mut Context<Self>) {
        if let Some(sess) = self.sess.as_mut().filter(|s| s.selected.is_some()) {
            for p in paths.into_iter().filter(|p| p.is_file()) {
                if !sess.pending.contains(&p) {
                    sess.pending.push(p);
                }
            }
            cx.notify();
        }
    }

    /// Ctrl+V in the composer: a clipboard image or copied files become attachments.
    /// Returns false when there are none, so the text paste goes on.
    pub fn paste_attachments(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(item) = cx.read_from_clipboard() else { return false };
        let mut paths: Vec<PathBuf> = item.entries().iter().filter_map(|e| if let ClipboardEntry::ExternalPaths(p) = e { Some(p.0.iter().cloned()) } else { None }).flatten().collect();
        // Linux file managers leave only the paths as text (no uri-list support in GPUI).
        if paths.is_empty() {
            paths = item.text().and_then(|t| clipboard_text_paths(&t)).unwrap_or_default();
        }
        // Apps that copy a file may also put its picture on the clipboard: the file alone is enough.
        if paths.is_empty() {
            paths.extend(item.entries().iter().filter_map(|e| if let ClipboardEntry::Image(img) = e { save_pasted(img) } else { None }));
        }
        if paths.is_empty() {
            return false;
        }
        self.add_pending(paths, cx);
        true
    }

    pub fn remove_pending(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some(sess) = self.sess.as_mut()
            && ix < sess.pending.len()
        {
            sess.pending.remove(ix);
            cx.notify();
        }
    }

    pub fn attach(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: true, prompt: None });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else { return };
            let _ = this.update(cx, |app, cx| app.add_pending(paths, cx));
        })
        .detach();
    }

    fn send_file(&mut self, peer: &str, path: PathBuf) {
        let Some(sess) = self.sess.as_ref() else { return };
        let peer = peer.to_string();
        let groupchat = sess.chat(&peer).is_some_and(|c| c.is_room);
        let sender = if groupchat { sess.chat(&peer).map(|c| c.nick.clone()).unwrap_or_default() } else { sess.me.clone() };
        let id = xmpp::new_id();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("файл").to_string();
        let Some(sess) = self.sess.as_mut() else { return };
        sess.uploads.insert(id.clone(), path.clone());
        let enc = sess.chat(&peer).map(|c| c.enc.clone()).unwrap_or_default();
        sess.send(Cmd::Upload { to: peer.clone(), id: id.clone(), path, groupchat, enc });
        self.push_outgoing(&peer, Msg { msg_id: id, sender, outgoing: true, body: format!("Отправка файла {name}..."), url: None, ts: now_ms(), status: 0, enc: 0, edited: false, reply_to: None, retracted: false });
    }

    /// Mic button: starts a voice message.
    pub fn start_recording(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.sess.as_ref().is_some_and(|s| s.call.is_some()) {
            return window.push_notification("Во время звонка микрофон занят", cx);
        }
        self.player = None; // don't record our own speaker
        match crabber::voice::Recorder::start() {
            Ok(r) => {
                self.recording = Some((r, Instant::now()));
                self.animate(cx);
            }
            Err(e) => window.push_notification(format!("Не удалось включить микрофон: {e}"), cx),
        }
        cx.notify();
    }

    pub fn cancel_recording(&mut self, cx: &mut Context<Self>) {
        self.recording = None;
        cx.notify();
    }

    /// Encodes the recording to Ogg Opus off the UI thread and sends it like a file.
    pub fn finish_recording(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((rec, _)) = self.recording.take() else { return };
        let Some(sess) = self.sess.as_ref() else { return };
        let Some(peer) = sess.selected.clone() else { return };
        let dir = sess.dir.join("voice");
        let pcm = rec.finish();
        cx.notify();
        if pcm.len() < 8000 {
            return; // under half a second: a misclick
        }
        // Exact zeros: a hardware mute or a dead input, not a quiet room (that still has noise).
        if pcm.iter().all(|s| *s == 0) {
            return window.push_notification("Микрофон передаёт только тишину — проверьте кнопку отключения звука на микрофоне и выбранное устройство ввода в настройках звука", cx);
        }
        let task = cx.background_executor().spawn(async move {
            let bytes = crabber::voice::encode(&pcm)?;
            std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
            let path = dir.join(format!("voice-{}.oga", chrono::Local::now().format("%Y%m%d-%H%M%S")));
            std::fs::write(&path, bytes).map_err(|e| e.to_string())?;
            Ok::<_, String>(path)
        });
        cx.spawn_in(window, async move |this, cx| {
            let res = task.await;
            let _ = this.update_in(cx, |app, window, cx| match res {
                Ok(path) => {
                    app.send_file(&peer, path);
                    cx.notify();
                }
                Err(e) => window.push_notification(format!("Не удалось сохранить запись: {e}"), cx),
            });
        })
        .detach();
    }

    /// Play/pause button of a voice message.
    pub fn toggle_play(&mut self, url: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.player.take().is_some_and(|(u, _)| u == url) {
            cx.notify();
            return;
        }
        let Some(sess) = self.sess.as_ref() else { return };
        let Some(path) = sess.store.file_for(&url).filter(|p| p.is_file()) else {
            if let Some(sess) = self.sess.as_mut() {
                sess.failed.remove(&url);
            }
            self.request_download(&url);
            cx.notify();
            return;
        };
        let task = cx.background_executor().spawn(async move { std::fs::read(&path).map_err(|e| e.to_string()).and_then(|b| crabber::voice::decode(&b)) });
        cx.spawn_in(window, async move |this, cx| {
            let res = task.await.and_then(crabber::voice::Player::start);
            let _ = this.update_in(cx, |app, window, cx| match res {
                Ok(player) => {
                    app.player = Some((url, player));
                    app.animate(cx);
                    cx.notify();
                }
                Err(e) => window.push_notification(format!("Не удалось воспроизвести: {e}"), cx),
            });
        })
        .detach();
    }

    /// Redraws every 100 ms while something moves (playback progress, recording timer).
    fn animate(&mut self, cx: &mut Context<Self>) {
        if std::mem::replace(&mut self.animating, true) {
            return;
        }
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_millis(100)).await;
                let go = this.update(cx, |app, cx| {
                    if app.player.as_ref().is_some_and(|(_, p)| p.finished()) {
                        app.player = None;
                    }
                    cx.notify();
                    let go = app.player.is_some() || app.recording.is_some();
                    app.animating = go;
                    go
                });
                if !matches!(go, Ok(true)) {
                    break;
                }
            }
        })
        .detach();
    }

    /// Smile button in the composer: the picked emoji goes in at the cursor.
    pub fn insert_emoji(&mut self, emoji: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.emoji_picker = None;
        let emoji = emoji.to_string();
        self.compose.update(cx, |s, cx| {
            s.insert(emoji, window, cx);
            s.focus(window, cx);
        });
        cx.notify();
    }

    /// Puts a picture on the clipboard (message menu and viewer menu).
    pub fn copy_image(&mut self, path: &std::path::Path, cx: &mut Context<Self>) {
        self.msg_menu = None;
        self.viewer_menu = None;
        if let Ok(bytes) = std::fs::read(path)
            && let Some(format) = image_format(&bytes)
        {
            cx.write_to_clipboard(ClipboardItem::new_image(&Image::from_bytes(format, bytes)));
        }
        cx.notify();
    }

    pub fn change_avatar(&mut self, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("Аватар".into()) });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(mut paths))) = paths.await else { return };
            let Some(path) = paths.pop() else { return };
            let _ = this.update(cx, |app, _| {
                if let Some(sess) = &app.sess {
                    sess.send(Cmd::SetAvatar(path));
                }
            });
        })
        .detach();
    }

    /// Starts a download once; the view re-checks the file on the next render.
    pub fn request_download(&mut self, url: &str) {
        let Some(sess) = self.sess.as_mut() else { return };
        if !sess.failed.contains(url) && sess.downloads.insert(url.to_string()) {
            let path = sess.store.reserve_file(&store::downloads_dir(), url);
            sess.send(Cmd::Download { url: url.to_string(), path });
        }
    }

    pub fn open_attachment(&mut self, url: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sess) = self.sess.as_ref() else { return };
        if let Some(path) = sess.store.file_for(&url).filter(|p| p.is_file()) {
            // Files from other people are previewed or shown in Explorer, never launched (could be an .exe).
            if is_image(&url) {
                let peer = sess.selected.clone().unwrap_or_default();
                let gallery = sess.store.attachments(&peer).into_iter().filter_map(|m| m.url).filter(|u| is_image(u)).collect();
                self.viewer = Some(Viewer::Image { path, url, gallery });
            } else if is_text(&url) {
                let bytes = std::fs::read(&path).unwrap_or_default();
                let text = String::from_utf8_lossy(&bytes[..bytes.len().min(512 * 1024)]).into_owned();
                self.viewer = Some(Viewer::Text { name: file_name(&url), path, text });
            } else {
                cx.reveal_path(&path);
                return;
            }
            self.viewer_focus.focus(window, cx);
            cx.notify();
        } else {
            if let Some(sess) = self.sess.as_mut() {
                sess.failed.remove(&url);
            }
            self.request_download(&url);
        }
    }

    /// "Save as": copies the downloaded file, or downloads straight to the chosen path.
    pub fn save_attachment_as(&mut self, url: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sess) = self.sess.as_ref() else { return };
        let src = sess.store.file_for(&url).filter(|p| p.is_file());
        let dir = dirs::download_dir().unwrap_or_else(store::data_dir);
        let dest = cx.prompt_for_new_path(&dir, Some(&file_name(&url)));
        cx.spawn_in(window, async move |this, cx| {
            let dest = match dest.await {
                Ok(Ok(Some(dest))) => dest,
                // Linux without xdg-desktop-portal: no dialog at all, so say why.
                Ok(Err(e)) => {
                    let _ = this.update_in(cx, |_, window, cx| window.push_notification(format!("Не удалось открыть диалог сохранения: {e}"), cx));
                    return;
                }
                _ => return,
            };
            let _ = this.update_in(cx, |app, window, cx| match src {
                Some(src) => {
                    if let Err(e) = std::fs::copy(&src, &dest) {
                        window.push_notification(format!("Не удалось сохранить: {e}"), cx);
                    }
                }
                None => {
                    if let Some(sess) = app.sess.as_mut() {
                        sess.failed.remove(&url);
                        sess.send(Cmd::Download { url, path: dest });
                    }
                }
            });
        })
        .detach();
    }

    pub fn cycle_show(&mut self, cx: &mut Context<Self>) {
        let Some(sess) = self.sess.as_mut() else { return };
        sess.show = match sess.show {
            None => Some(Show::Away),
            Some(Show::Away) => Some(Show::Dnd),
            _ => None,
        };
        sess.send(Cmd::SetShow(sess.show.clone()));
        cx.notify();
    }

    pub fn answer_request(&mut self, jid: String, accept: bool, cx: &mut Context<Self>) {
        let Some(sess) = self.sess.as_mut() else { return };
        sess.requests.retain(|j| *j != jid);
        if accept {
            sess.send(Cmd::Approve { jid: jid.clone() });
            sess.send(Cmd::AddContact { jid, name: String::new() });
        } else {
            sess.send(Cmd::Deny { jid });
        }
        cx.notify();
    }

    pub fn open_modal(&mut self, modal: Modal, window: &mut Window, cx: &mut Context<Self>) {
        let (p1, p2) = match &modal {
            Modal::AddContact => ("Адрес контакта, например bob@example.com", "Имя (необязательно)"),
            Modal::NewGroup => ("", "Название группы"),
            Modal::JoinGroup => ("", "Адрес группы, например room@conference.example.com"),
            _ => ("", ""),
        };
        self.field1.update(cx, |s, cx| {
            s.set_value("", window, cx);
            s.set_placeholder(p1, window, cx);
        });
        self.field2.update(cx, |s, cx| {
            s.set_value("", window, cx);
            s.set_placeholder(p2, window, cx);
        });
        let focus = if matches!(modal, Modal::AddContact) { &self.field1 } else { &self.field2 };
        focus.update(cx, |s, cx| s.focus(window, cx));
        self.modal = Some(modal);
        cx.notify();
    }

    pub fn close_modal(&mut self, cx: &mut Context<Self>) {
        self.modal = None;
        cx.notify();
    }

    pub fn submit_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let v1 = self.field1.read(cx).value().trim().to_string();
        let v2 = self.field2.read(cx).value().trim().to_string();
        let Some(sess) = self.sess.as_mut() else { return };
        match self.modal.take() {
            Some(Modal::AddContact) => {
                if BareJid::from_str(&v1).is_err() || !v1.contains('@') {
                    window.push_notification("Неверный адрес контакта", cx);
                    self.modal = Some(Modal::AddContact);
                    return;
                }
                sess.send(Cmd::AddContact { jid: v1.clone(), name: v2.clone() });
                let chat = sess.ensure_chat(&v1, false);
                chat.name = v2;
                chat.in_roster = true;
                sess.save(&v1);
                self.select(v1, window, cx);
            }
            Some(Modal::NewGroup) => {
                if v2.is_empty() {
                    self.modal = Some(Modal::NewGroup);
                    return;
                }
                let slug: String = v2.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).take(20).collect();
                let id = xmpp::new_id();
                // shortcut: assumes the MUC service is conference.<domain> (ejabberd default); disco#items would find it on any server.
                let room = format!("{}{}@conference.{}", slug, &id[id.len() - 6..], sess.domain());
                let nick = sess.localpart();
                let chat = sess.ensure_chat(&room, true);
                chat.name = v2.clone();
                sess.save(&room);
                sess.send(Cmd::Join { room: room.clone(), nick, since: None, create_name: Some(v2) });
                sess.publish_bookmark(&room);
                self.select(room, window, cx);
            }
            Some(Modal::JoinGroup) => {
                if BareJid::from_str(&v2).is_err() || !v2.contains('@') {
                    window.push_notification("Неверный адрес группы", cx);
                    self.modal = Some(Modal::JoinGroup);
                    return;
                }
                let nick = sess.localpart();
                sess.ensure_chat(&v2, true);
                sess.save(&v2);
                sess.send(Cmd::Join { room: v2.clone(), nick, since: sess.store.last_ts(&v2), create_name: None });
                sess.publish_bookmark(&v2);
                self.select(v2, window, cx);
            }
            other => self.modal = other,
        }
        cx.notify();
    }

    pub fn invite(&mut self, room: String, jid: String, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(sess) = &self.sess {
            sess.send(Cmd::Invite { room, jid: jid.clone() });
            window.push_notification(format!("Приглашение отправлено: {jid}"), cx);
        }
        self.close_modal(cx);
    }

    /// "Delete for me": gone from this device only.
    pub fn delete_message(&mut self, msg_id: &str, cx: &mut Context<Self>) {
        self.msg_menu = None;
        let Some(sess) = self.sess.as_mut() else { return };
        let Some(peer) = sess.selected.clone() else { return };
        sess.store.delete_msg(&peer, msg_id);
        if let Some(ix) = sess.history.iter().position(|m| m.msg_id == msg_id) {
            sess.history.remove(ix);
            self.messages.splice(ix..ix + 1, 0);
        }
        let last = sess.store.last_msg(&peer);
        if let Some(c) = sess.chat_mut(&peer) {
            c.last = last;
        }
        cx.notify();
    }

    /// Full-window view of a contact's avatar.
    pub fn open_avatar(&mut self, peer: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sess) = self.sess.as_ref() else { return };
        let path = store::avatar_path(&sess.dir, peer);
        if path.is_file() {
            self.viewer = Some(Viewer::Image { path, url: peer.to_string(), gallery: vec![] });
            self.viewer_focus.focus(window, cx);
            cx.notify();
        }
    }

    pub fn delete_chat(&mut self, peer: String, cx: &mut Context<Self>) {
        let Some(sess) = self.sess.as_mut() else { return };
        let is_room = sess.chat(&peer).is_some_and(|c| c.is_room);
        if is_room {
            sess.send(Cmd::Leave { room: peer.clone() });
        } else {
            sess.send(Cmd::RemoveContact { jid: peer.clone() });
        }
        sess.store.delete_chat(&peer);
        sess.chats.retain(|c| c.peer != peer);
        if sess.selected.as_deref() == Some(&peer) {
            sess.selected = None;
            sess.history.clear();
            self.messages.reset(0);
        }
        self.modal = None;
        cx.notify();
    }

    pub fn call(&mut self, cmd: CallCmd, cx: &mut Context<Self>) {
        let Some(sess) = self.sess.as_mut() else { return };
        let mut ended = None;
        match &cmd {
            CallCmd::Start { to } => {
                if sess.call.is_some() {
                    return;
                }
                sess.call = Some(CallUi { peer: to.clone(), state: CallState::Outgoing, muted: false, started: None, outgoing: true });
            }
            CallCmd::Mute(m) => {
                if let Some(c) = sess.call.as_mut() {
                    c.muted = *m;
                }
            }
            CallCmd::Hangup => ended = sess.call.take(),
            CallCmd::Accept => {
                if let Some(c) = sess.call.as_mut() {
                    c.state = CallState::Connecting;
                }
            }
        }
        sess.send(Cmd::Call(cmd));
        if let Some(c) = ended {
            self.log_call(c);
        }
        cx.notify();
    }

    /// Adds a local call line to the chat history.
    fn log_call(&mut self, c: CallUi) {
        let Some(me) = self.sess.as_ref().map(|s| s.me.clone()) else { return };
        let body = match c.started {
            Some(t) => {
                let s = t.elapsed().as_secs();
                format!("{} звонок · {:02}:{:02}", if c.outgoing { "Исходящий" } else { "Входящий" }, s / 60, s % 60)
            }
            None if c.outgoing => "Исходящий звонок · без ответа".to_string(),
            None => "Пропущенный звонок".to_string(),
        };
        let sender = if c.outgoing { me } else { c.peer.clone() };
        let msg = Msg { msg_id: format!("{CALL_ID}{}", xmpp::new_id()), sender, outgoing: c.outgoing, body, url: None, ts: now_ms(), status: 2, enc: 0, edited: false, reply_to: None, retracted: false };
        self.push_outgoing(&c.peer, msg);
    }
}

/// Id prefix of local call lines: they are never sent, ticked or marked read.
const CALL_ID: &str = "call-";

pub fn is_call(m: &Msg) -> bool {
    m.msg_id.starts_with(CALL_ID)
}

/// Clears the unread counter and sends a read marker for the newest incoming message.
fn read_chat(sess: &mut Session, peer: &str) {
    let Some(chat) = sess.chat_mut(peer) else { return };
    let is_room = chat.is_room;
    if chat.unread > 0 {
        chat.unread = 0;
        sess.save(peer);
    }
    // XEP-0490: tell our other devices how far we read.
    if let Some(sid) = sess.store.last_sid(peer)
        && sess.mds.get(peer) != Some(&sid)
    {
        sess.mds.insert(peer.to_string(), sid.clone());
        sess.send(Cmd::MarkDisplayed { peer: peer.to_string(), sid, groupchat: is_room });
    }
    if is_room {
        return;
    }
    let last = if sess.selected.as_deref() == Some(peer) {
        sess.history.iter().rev().find(|m| !m.outgoing && !is_call(m)).map(|m| m.msg_id.clone())
    } else {
        sess.store.history(peer, 50).into_iter().rev().find(|m| !m.outgoing && !is_call(m)).map(|m| m.msg_id)
    };
    if let Some(id) = last
        && sess.displayed_sent.get(peer) != Some(&id)
    {
        sess.displayed_sent.insert(peer.to_string(), id.clone());
        sess.send(Cmd::Displayed { to: peer.to_string(), id });
    }
}

pub fn file_name(url: &str) -> String {
    url.split(['?', '#']).next().unwrap_or(url).rsplit('/').next().unwrap_or("файл").to_string()
}

pub fn ext(url: &str) -> String {
    file_name(url).rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}

/// Files shown as text: previewed in the chat and in the viewer.
pub fn is_text(url: &str) -> bool {
    matches!(
        ext(url).as_str(),
        "txt" | "md" | "log" | "csv" | "json" | "toml" | "yaml" | "yml" | "xml" | "ini" | "rs" | "py" | "js" | "ts" | "tsx" | "jsx" | "c" | "h" | "cpp" | "hpp" | "cs" | "go" | "java" | "kt" | "rb" | "php" | "sh" | "ps1" | "bat" | "sql" | "html" | "css" | "lua" | "swift"
    )
}

fn apply_contact(sess: &mut Session, c: xmpp::Contact) {
    // A request that arrived before the roster: it's someone we added ourselves, so approve.
    if (c.subscription == "to" || c.subscription == "none") && sess.requests.contains(&c.jid) {
        sess.requests.retain(|j| *j != c.jid);
        sess.send(Cmd::Approve { jid: c.jid.clone() });
    }
    if c.subscription == "remove" {
        if let Some(chat) = sess.chat_mut(&c.jid) {
            chat.in_roster = false;
        }
        sess.save(&c.jid);
        return;
    }
    let chat = sess.ensure_chat(&c.jid, false);
    chat.in_roster = true;
    chat.subscription = c.subscription;
    if !c.name.is_empty() {
        chat.name = c.name;
    }
    sess.save(&c.jid);
}

/// Our role and affiliation in `room`.
pub fn my_occupant<'a>(sess: &'a Session, room: &str) -> Option<&'a Occupant> {
    let nick = &sess.chat(room)?.nick;
    sess.members.get(room)?.get(nick)
}

fn apply_retract(sess: &mut Session, peer: &str, msg_id: &str, sender: Option<&str>) {
    if !sess.store.retract(peer, msg_id, sender) {
        return;
    }
    let blank = |m: &mut Msg| {
        if m.msg_id == msg_id {
            m.body.clear();
            m.url = None;
            m.retracted = true;
        }
    };
    update_history(sess, peer, blank);
    if let Some(last) = sess.chat_mut(peer).and_then(|c| c.last.as_mut()) {
        blank(last);
    }
    if sess.selected.as_deref() == Some(peer) {
        sess.reactions.remove(msg_id);
    }
}

/// Time of the message our XEP-0490 read marker points at, if we have it.
fn read_marker_ts(sess: &Session, peer: &str) -> Option<i64> {
    let sid = sess.mds.get(peer)?;
    let id = sess.store.resolve(peer, sid)?;
    sess.store.msg(peer, &id).map(|m| m.ts)
}

/// XEP-0461 reference to `r` plus the quoted fallback for clients without replies.
fn reply_for(sess: &Session, peer: &str, groupchat: bool, r: &Msg) -> Reply {
    let (id, to) = if groupchat {
        (sess.store.sid(peer, &r.msg_id).unwrap_or_else(|| r.msg_id.clone()), format!("{peer}/{}", r.sender))
    } else {
        (r.msg_id.clone(), if r.outgoing { sess.me.clone() } else { peer.to_string() })
    };
    let name = if groupchat { r.sender.clone() } else if r.outgoing { sess.localpart() } else { sess.display_name(peer) };
    let text: String = r.url.clone().unwrap_or_else(|| r.body.clone()).lines().take(3).collect::<Vec<_>>().join("\n").chars().take(200).collect();
    let quote = format!("> {name}: {}\n", text.replace('\n', "\n> "));
    Reply { id, to, quote }
}

fn apply_correction(sess: &mut Session, peer: &str, id: &str, sender: &str, body: &str) {
    if !sess.store.correct(peer, id, sender, body) {
        return;
    }
    let fix = |m: &mut Msg| {
        if m.msg_id == id {
            m.body = body.to_string();
            m.edited = true;
        }
    };
    update_history(sess, peer, fix);
    if let Some(last) = sess.chat_mut(peer).and_then(|c| c.last.as_mut()) {
        fix(last);
    }
}

/// A bookmarked room (from the server) we are not in yet: add and join it.
fn add_bookmark(sess: &mut Session, b: Bookmark) {
    if sess.chat(&b.room).is_some() {
        return;
    }
    let nick = if b.nick.is_empty() { sess.localpart() } else { b.nick };
    let chat = sess.ensure_chat(&b.room, true);
    chat.name = b.name;
    chat.nick = nick.clone();
    sess.save(&b.room);
    let since = sess.store.last_ts(&b.room);
    sess.send(Cmd::Join { room: b.room, nick, since, create_name: None });
}

/// Room left on another device: leave here too. History stays in the database for a rejoin.
fn forget_room(sess: &mut Session, messages: &mut ListState, room: &str) {
    sess.send(Cmd::Leave { room: room.to_string() });
    sess.store.forget_chat(room);
    sess.chats.retain(|c| c.peer != room);
    if sess.selected.as_deref() == Some(room) {
        sess.selected = None;
        sess.history.clear();
        messages.reset(0);
    }
}

fn update_history(sess: &mut Session, peer: &str, f: impl FnMut(&mut Msg)) {
    if sess.selected.as_deref() == Some(peer) {
        sess.history.iter_mut().for_each(f);
    }
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    #[test]
    fn encrypted_links_keep_name_and_type() {
        let url = "aesgcm://up.example.org/abc/image-1.png#00ff";
        assert!(super::is_image(url));
        assert_eq!(super::file_name(url), "image-1.png");
        assert_eq!(super::ext("https://h/a/doc.PDF?x=1"), "pdf");
    }

    #[test]
    fn clipboard_text_becomes_paths_only_when_every_line_is_a_file() {
        let dir = std::env::temp_dir();
        let a = dir.join(format!("crabber-paste-{}-a.txt", std::process::id()));
        let b = dir.join(format!("crabber-paste-{}-b.txt", std::process::id()));
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"y").unwrap();
        assert_eq!(super::clipboard_text_paths(&format!("{}\r\n{}", a.display(), b.display())).unwrap(), vec![a.clone(), b.clone()]);
        // Mixed text, missing files and empty input stay a plain paste.
        assert!(super::clipboard_text_paths(&format!("{}\nне файл", a.display())).is_none());
        assert!(super::clipboard_text_paths(&format!("{} 2", a.display())).is_none());
        assert!(super::clipboard_text_paths("привет").is_none());
        assert!(super::clipboard_text_paths("").is_none());
        #[cfg(unix)]
        {
            let spaced = dir.join(format!("crabber paste {}.png", std::process::id()));
            std::fs::write(&spaced, b"z").unwrap();
            let url = format!("file://{}", spaced.display().to_string().replace(' ', "%20"));
            assert_eq!(super::clipboard_text_paths(&url).unwrap(), vec![spaced.clone()]);
            assert!(super::clipboard_text_paths("file://host/share.txt").is_none());
            let _ = std::fs::remove_file(&spaced);
        }
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    #[test]
    fn percent_decode_handles_escapes_and_leaves_rest() {
        assert_eq!(super::percent_decode("/a%20b/%D1%84.txt"), "/a b/ф.txt");
        assert_eq!(super::percent_decode("/100%.txt"), "/100%.txt");
        assert_eq!(super::percent_decode("/trailing%2"), "/trailing%2");
    }
}
