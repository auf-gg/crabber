//! Desktop integration: tray icon, notifications (Windows toasts with inline reply, XDG on Linux), hide-to-tray.
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};

#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
pub enum DesktopEvent {
    Show,
    /// Left click on the tray icon: hide a shown window, show a hidden one.
    Toggle,
    Quit,
    Open(String),
    #[cfg_attr(not(windows), allow(dead_code))] // inline reply exists only in Windows toasts
    Reply(String, String),
}

/// The app icon (assets/crabber.png: icon.png with transparent corners), bundled into the binary.
pub const ICON_PNG: &[u8] = include_bytes!("../assets/crabber.png");

/// RGBA app icon at `size`, optionally with a red unread dot in the corner.
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
pub fn app_icon(size: u32, badge: bool) -> Vec<u8> {
    let icon = image::load_from_memory_with_format(ICON_PNG, image::ImageFormat::Png).expect("bundled icon").to_rgba8();
    let mut icon = image::imageops::resize(&icon, size, size, image::imageops::FilterType::Lanczos3);
    if badge {
        let s = size as f32;
        let (cx, cy, r) = (s * 0.78, s * 0.22, s * 0.22);
        for (x, y, px) in icon.enumerate_pixels_mut() {
            let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
            if d <= r {
                *px = image::Rgba([0xE5, 0x39, 0x35, 0xFF]);
            } else if d <= r + s * 0.06 {
                // Dark ring so the dot stays visible over the orange crab.
                *px = image::Rgba([0x1B, 0x1B, 0x1F, 0xFF]);
            }
        }
    }
    icon.into_raw()
}

#[cfg(windows)]
mod imp {
    use std::sync::{Mutex, OnceLock};

    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::Foundation::{IPropertyValue, TypedEventHandler};
    use windows::UI::Notifications::{ToastActivatedEventArgs, ToastNotification, ToastNotificationManager, ToastNotifier};
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ, RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW};
    use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
    use windows::Win32::UI::WindowsAndMessaging::{IsIconic, IsWindowVisible, SW_HIDE, SW_RESTORE, SetForegroundWindow, ShowWindow};
    use windows::core::{HSTRING, IInspectable, Interface};

    use super::{DesktopEvent, UnboundedSender, app_icon};

    const AUMID: &str = "Crabber.Messenger";

    static HWND_RAW: OnceLock<isize> = OnceLock::new();
    static NOTIFIER: OnceLock<Option<ToastNotifier>> = OnceLock::new();
    static EVENTS: OnceLock<UnboundedSender<DesktopEvent>> = OnceLock::new();
    /// Toasts must stay alive for their Activated handler to fire.
    static LIVE: Mutex<Vec<ToastNotification>> = Mutex::new(Vec::new());

    pub struct Tray {
        icon: TrayIcon,
        badge: bool,
    }

    impl Tray {
        pub fn set_unread(&mut self, unread: u32) {
            let badge = unread > 0;
            let tip = if badge { format!("Crabber — непрочитанных: {unread}") } else { "Crabber".into() };
            let _ = self.icon.set_tooltip(Some(tip));
            if badge != self.badge {
                self.badge = badge;
                let _ = self.icon.set_icon(Icon::from_rgba(app_icon(32, badge), 32, 32).ok());
            }
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Windows build number (22000+ is Windows 11), 0 if unknown.
    pub fn build() -> u32 {
        static BUILD: OnceLock<u32> = OnceLock::new();
        *BUILD.get_or_init(|| {
            use windows::Win32::System::Registry::HKEY_LOCAL_MACHINE;
            let (key, name) = (wide("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion"), wide("CurrentBuildNumber"));
            let mut buf = [0u16; 32];
            let mut len = (buf.len() * 2) as u32;
            let ok = unsafe {
                RegGetValueW(HKEY_LOCAL_MACHINE, windows::core::PCWSTR(key.as_ptr()), windows::core::PCWSTR(name.as_ptr()), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as *mut _), Some(&mut len))
            }
            .is_ok();
            if !ok {
                return 0;
            }
            // `len` is in bytes and counts the terminating NUL.
            String::from_utf16_lossy(&buf[..(len as usize / 2).saturating_sub(1)]).parse().unwrap_or(0)
        })
    }

    const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

    /// Launch at login: a value under the per-user Run key.
    pub fn autostart() -> bool {
        let (key, name) = (wide(RUN_KEY), wide("Crabber"));
        unsafe { RegGetValueW(HKEY_CURRENT_USER, windows::core::PCWSTR(key.as_ptr()), windows::core::PCWSTR(name.as_ptr()), RRF_RT_REG_SZ, None, None, None).is_ok() }
    }

    pub fn set_autostart(on: bool) {
        let (key, name) = (wide(RUN_KEY), wide("Crabber"));
        let (key, name) = (windows::core::PCWSTR(key.as_ptr()), windows::core::PCWSTR(name.as_ptr()));
        unsafe {
            if !on {
                let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, key, name);
                return;
            }
            let Ok(exe) = std::env::current_exe() else { return };
            let data = wide(&format!("\"{}\"", exe.display()));
            let _ = RegSetKeyValueW(HKEY_CURRENT_USER, key, name, REG_SZ.0, Some(data.as_ptr() as *const _), (data.len() * 2) as u32);
        }
    }

    /// Registers the app identity so toasts show our name and icon (no installer/shortcut needed).
    fn register_aumid(icon: &std::path::Path) {
        let key = wide(&format!("Software\\Classes\\AppUserModelId\\{AUMID}"));
        for (name, value) in [("DisplayName", "Crabber".to_string()), ("IconUri", icon.display().to_string().replace('/', "\\"))] {
            let data = wide(&value);
            unsafe {
                let _ = RegSetKeyValueW(
                    HKEY_CURRENT_USER,
                    windows::core::PCWSTR(key.as_ptr()),
                    windows::core::PCWSTR(wide(name).as_ptr()),
                    REG_SZ.0,
                    Some(data.as_ptr() as *const _),
                    (data.len() * 2) as u32,
                );
            }
        }
        unsafe {
            let _ = SetCurrentProcessExplicitAppUserModelID(&HSTRING::from(AUMID));
        }
    }

    pub fn init(tx: UnboundedSender<DesktopEvent>, data_dir: &std::path::Path) -> Option<Tray> {
        let icon_path = data_dir.join("icon.png");
        let _ = image::save_buffer(&icon_path, &app_icon(256, false), 256, 256, image::ColorType::Rgba8);
        register_aumid(&icon_path);
        let _ = EVENTS.set(tx.clone());

        let menu = Menu::new();
        let open = MenuItem::new("Открыть", true, None);
        let quit = MenuItem::new("Выход", true, None);
        let _ = menu.append_items(&[&open, &PredefinedMenuItem::separator(), &quit]);
        let (open_id, quit_id) = (open.id().clone(), quit.id().clone());
        let menu_tx = tx.clone();
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            let ev = if e.id == quit_id { DesktopEvent::Quit } else if e.id == open_id { DesktopEvent::Show } else { return };
            let _ = menu_tx.unbounded_send(ev);
        }));
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                let _ = tx.unbounded_send(DesktopEvent::Toggle);
            }
        }));
        let icon = TrayIconBuilder::new()
            .with_icon(Icon::from_rgba(app_icon(32, false), 32, 32).ok()?)
            .with_tooltip("Crabber")
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .build()
            .ok()?;
        Some(Tray { icon, badge: false })
    }

    pub fn set_window(window: &gpui_kit::Window) {
        use raw_window_handle::{HasWindowHandle, RawWindowHandle};
        if let Ok(h) = HasWindowHandle::window_handle(window)
            && let RawWindowHandle::Win32(w) = h.as_raw()
        {
            let _ = HWND_RAW.set(w.hwnd.get());
        }
    }

    fn hwnd() -> Option<HWND> {
        HWND_RAW.get().map(|h| HWND(*h as *mut _))
    }

    pub fn hide_window(_: &mut gpui_kit::Window) {
        if let Some(h) = hwnd() {
            unsafe {
                let _ = ShowWindow(h, SW_HIDE);
            }
        }
    }

    pub fn show_window(_: &mut gpui_kit::Window) {
        if let Some(h) = hwnd() {
            unsafe {
                let _ = ShowWindow(h, SW_RESTORE);
                let _ = SetForegroundWindow(h);
            }
        }
    }

    pub fn toggle_window(window: &mut gpui_kit::Window) {
        let shown = hwnd().is_some_and(|h| unsafe { IsWindowVisible(h).as_bool() && !IsIconic(h).as_bool() });
        if shown { hide_window(window) } else { show_window(window) }
    }

    fn esc(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
    }

    /// Shows a toast; clicking opens the chat, the text box replies without opening the app.
    pub fn notify(peer: &str, title: &str, body: &str, avatar: Option<&std::path::Path>) {
        let Some(notifier) = NOTIFIER.get_or_init(|| ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID)).ok()) else {
            return;
        };
        let image = avatar
            .filter(|p| p.exists())
            .map(|p| format!(r#"<image placement="appLogoOverride" hint-crop="circle" src="{}"/>"#, esc(&p.display().to_string().replace('/', "\\"))))
            .unwrap_or_default();
        let body: String = body.chars().take(300).collect();
        let xml = format!(
            r#"<toast launch="open:{peer}"><visual><binding template="ToastGeneric"><text>{title}</text><text>{body}</text>{image}</binding></visual><actions><input id="reply" type="text" placeHolderContent="Ответить..."/><action content="Отправить" arguments="reply:{peer}" hint-inputId="reply" activationType="foreground"/></actions></toast>"#,
            peer = esc(peer),
            title = esc(title),
            body = esc(&body),
        );
        let show = || -> windows::core::Result<()> {
            let doc = XmlDocument::new()?;
            doc.LoadXml(&HSTRING::from(xml))?;
            let toast = ToastNotification::CreateToastNotification(&doc)?;
            // One toast per chat: a newer message replaces the older one.
            let tag: String = format!("{:x}", std::hash::BuildHasher::hash_one(&std::hash::BuildHasherDefault::<std::collections::hash_map::DefaultHasher>::default(), peer));
            toast.SetTag(&HSTRING::from(tag))?;
            toast.Activated(&TypedEventHandler::<ToastNotification, IInspectable>::new(|_, args| {
                let Some(args) = args.as_ref() else { return Ok(()) };
                let args: ToastActivatedEventArgs = args.cast()?;
                let arguments = args.Arguments()?.to_string();
                let ev = if let Some(peer) = arguments.strip_prefix("reply:") {
                    let text = args
                        .UserInput()
                        .and_then(|input| input.Lookup(&HSTRING::from("reply")))
                        .and_then(|v| v.cast::<IPropertyValue>()?.GetString())
                        .map(|s| s.to_string())
                        .unwrap_or_default();
                    if text.trim().is_empty() {
                        DesktopEvent::Open(peer.to_string())
                    } else {
                        DesktopEvent::Reply(peer.to_string(), text)
                    }
                } else if let Some(peer) = arguments.strip_prefix("open:") {
                    DesktopEvent::Open(peer.to_string())
                } else {
                    DesktopEvent::Show
                };
                if let Some(tx) = EVENTS.get() {
                    let _ = tx.unbounded_send(ev);
                }
                Ok(())
            }))?;
            notifier.Show(&toast)?;
            let mut live = LIVE.lock().unwrap();
            live.push(toast);
            // shortcut: keeps the last 50 toasts' handlers alive; older ones lose reply support.
            if live.len() > 50 {
                live.remove(0);
            }
            Ok(())
        };
        if let Err(e) = show() {
            eprintln!("toast failed: {e}");
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::sync::OnceLock;

    use ksni::blocking::{Handle, TrayMethods};

    use super::{DesktopEvent, UnboundedSender, app_icon};

    static EVENTS: OnceLock<UnboundedSender<DesktopEvent>> = OnceLock::new();
    static ICON: OnceLock<String> = OnceLock::new();

    /// Launch at login: an XDG autostart entry.
    fn autostart_file() -> Option<std::path::PathBuf> {
        dirs::config_dir().map(|d| d.join("autostart").join("crabber.desktop"))
    }

    pub fn autostart() -> bool {
        autostart_file().is_some_and(|f| f.is_file())
    }

    pub fn set_autostart(on: bool) {
        let Some(file) = autostart_file() else { return };
        if !on {
            let _ = std::fs::remove_file(&file);
            return;
        }
        let Ok(exe) = std::env::current_exe() else { return };
        let entry = format!("[Desktop Entry]\nType=Application\nName=Crabber\nExec=\"{}\"\nIcon=crabber\nTerminal=false\nX-GNOME-Autostart-enabled=true\n", exe.display());
        let _ = file.parent().map(std::fs::create_dir_all);
        let _ = std::fs::write(&file, entry);
    }

    fn send(ev: DesktopEvent) {
        if let Some(tx) = EVENTS.get() {
            let _ = tx.unbounded_send(ev);
        }
    }

    /// StatusNotifierItem (KDE, GNOME with AppIndicator extension, most Wayland bars).
    struct Sni {
        unread: u32,
    }

    impl ksni::Tray for Sni {
        fn id(&self) -> String {
            "crabber".into()
        }
        fn title(&self) -> String {
            "Crabber".into()
        }
        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            let mut data = app_icon(32, self.unread > 0);
            data.chunks_exact_mut(4).for_each(|px| px.rotate_right(1)); // RGBA -> ARGB
            vec![ksni::Icon { width: 32, height: 32, data }]
        }
        fn tool_tip(&self) -> ksni::ToolTip {
            let title = if self.unread > 0 { format!("Crabber — непрочитанных: {}", self.unread) } else { "Crabber".into() };
            ksni::ToolTip { title, ..Default::default() }
        }
        fn activate(&mut self, _: i32, _: i32) {
            send(DesktopEvent::Toggle);
        }
        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            use ksni::menu::StandardItem;
            vec![
                StandardItem { label: "Открыть".into(), activate: Box::new(|_| send(DesktopEvent::Show)), ..Default::default() }.into(),
                ksni::MenuItem::Separator,
                StandardItem { label: "Выход".into(), activate: Box::new(|_| send(DesktopEvent::Quit)), ..Default::default() }.into(),
            ]
        }
    }

    pub struct Tray {
        handle: Handle<Sni>,
        unread: u32,
    }

    impl Tray {
        pub fn set_unread(&mut self, unread: u32) {
            if unread != self.unread {
                self.unread = unread;
                self.handle.update(|t| t.unread = unread);
            }
        }
    }

    pub fn init(tx: UnboundedSender<DesktopEvent>, data_dir: &std::path::Path) -> Option<Tray> {
        let icon_path = data_dir.join("icon.png");
        let _ = image::save_buffer(&icon_path, &app_icon(256, false), 256, 256, image::ColorType::Rgba8);
        let _ = ICON.set(icon_path.display().to_string());
        let _ = EVENTS.set(tx);
        // No SNI host (e.g. stock GNOME) -> no tray, so closing the window quits as before.
        match (Sni { unread: 0 }).spawn() {
            Ok(handle) => Some(Tray { handle, unread: 0 }),
            Err(e) => {
                eprintln!("tray unavailable: {e}");
                None
            }
        }
    }

    pub fn set_window(_: &gpui_kit::Window) {}

    // Wayland has no client-side window hide, and compositors without a minimize concept
    // (e.g. Hyprland) ignore `set_minimized`. Closing the window is the only real hide;
    // the app entity survives in the tray and app.rs `open_main_window` re-creates it.
    pub fn hide_window(window: &mut gpui_kit::Window) {
        window.remove_window();
    }

    // Only called while the window exists; the desktop event loop re-creates it otherwise.
    pub fn show_window(window: &mut gpui_kit::Window) {
        window.activate_window();
    }

    // The window only exists while shown, so an existing window is always the "shown" state.
    pub fn toggle_window(window: &mut gpui_kit::Window) {
        hide_window(window);
    }

    /// Desktop notification; clicking it opens the chat. Inline reply isn't part of the XDG spec.
    pub fn notify(peer: &str, title: &str, body: &str, avatar: Option<&std::path::Path>) {
        let body: String = body.chars().take(300).collect();
        let icon = avatar.filter(|p| p.exists()).map(|p| p.display().to_string()).or_else(|| ICON.get().cloned()).unwrap_or_default();
        let (peer, title) = (peer.to_string(), title.to_string());
        // wait_for_action blocks until the notification is clicked or closed.
        std::thread::spawn(move || {
            let shown = notify_rust::Notification::new()
                .appname("Crabber")
                .summary(&title)
                .body(&body)
                .icon(&icon)
                .action("default", "Открыть")
                .show();
            match shown {
                Ok(handle) => handle.wait_for_action(|action| {
                    if action == "default" {
                        send(DesktopEvent::Open(peer));
                    }
                }),
                Err(e) => eprintln!("notification failed: {e}"),
            }
        });
    }
}

#[cfg(not(any(windows, target_os = "linux")))]
mod imp {
    use super::{DesktopEvent, UnboundedSender};
    pub struct Tray;
    impl Tray {
        pub fn set_unread(&mut self, _: u32) {}
    }
    pub fn init(_: UnboundedSender<DesktopEvent>, _: &std::path::Path) -> Option<Tray> {
        None
    }
    pub fn set_window(_: &gpui_kit::Window) {}
    pub fn hide_window(_: &mut gpui_kit::Window) {}
    pub fn show_window(_: &mut gpui_kit::Window) {}
    pub fn toggle_window(_: &mut gpui_kit::Window) {}
    pub fn notify(_: &str, _: &str, _: &str, _: Option<&std::path::Path>) {}
    pub fn autostart() -> bool {
        false
    }
    pub fn set_autostart(_: bool) {}
}

/// Whether the system font can draw `e`; otherwise it shows as letters or empty boxes.
pub fn emoji_supported(e: &emojis::Emoji) -> bool {
    #[cfg(windows)]
    {
        // Segoe UI Emoji: Emoji 12.0 on Windows 10, ~15.0 on Windows 11, and no country flags on either.
        let max = if imp::build() >= 22000 { emojis::EmojiVersion::new(15, 0) } else { emojis::EmojiVersion::new(12, 0) };
        let flag = e.as_str().chars().any(|c| ('\u{1F1E6}'..='\u{1F1FF}').contains(&c) || ('\u{E0020}'..='\u{E007F}').contains(&c));
        e.emoji_version() <= max && !flag
    }
    #[cfg(not(windows))]
    {
        let _ = e;
        true
    }
}

pub use imp::{Tray, autostart, hide_window, notify, set_autostart, set_window, show_window, toggle_window};

/// The sender lets the app raise its own events (an incoming call shows the window).
pub fn init(data_dir: &std::path::Path) -> (Option<Tray>, UnboundedSender<DesktopEvent>, UnboundedReceiver<DesktopEvent>) {
    let (tx, rx) = unbounded();
    (imp::init(tx.clone(), data_dir), tx, rx)
}

#[cfg(test)]
mod tests {
    use super::app_icon;

    #[test]
    fn tray_icon_size_and_badge() {
        let plain = app_icon(32, false);
        let badged = app_icon(32, true);
        assert_eq!(plain.len(), 32 * 32 * 4);
        let px = |v: &[u8], x: usize, y: usize| v[(y * 32 + x) * 4..(y * 32 + x) * 4 + 4].to_vec();
        assert_eq!(px(&badged, 25, 7), [0xE5, 0x39, 0x35, 0xFF], "red unread dot");
        assert_eq!(px(&plain, 0, 0)[3], 0, "transparent rounded corner");
    }

    #[cfg(windows)]
    #[test]
    fn emoji_filter_on_windows() {
        let ok = |s: &str| super::emoji_supported(emojis::get(s).unwrap());
        assert!(super::imp::build() > 10000, "build number read from the registry");
        assert!(ok("🤝") && ok("🏁"));
        assert!(!ok("🇷🇺") && !ok("🏴󠁧󠁢󠁥󠁮󠁧󠁿"), "country flags render as letters");
        assert_eq!(ok("🫠"), super::imp::build() >= 22000, "Emoji 14.0 only on Windows 11");
    }
}
