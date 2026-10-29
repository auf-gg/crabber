// No console window in release builds.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod desktop;
mod style;
mod view;

use gpui_kit::*;

// Component defaults plus the Lucide icons this app uses.
gpui_kit::assets::icon_assets!(
    AppAssets,
    [Phone, PhoneOff, Pencil, Plus, FaceSlightlySmiling, Play, Pause, Reply, Ban, TriangleAlert, Paperclip, Mic, MicOff, UserPlus, UserMinus, Users, LogOut, SendHorizontal, Check, CheckCheck, File, Download, Hash, DoorOpen, FileImage, FileArchive, FileMusic, FileVideoCamera, FileText, FileCode, Copy, X, FolderOpen, Settings, Image, ChevronLeft, ChevronRight, ChevronDown, Link, Lock, LockOpen, Shield, ShieldCheck, Key, Search, Shell, Fish, Anchor, Sailboat, Shrimp, Snail, Turtle]
);

fn main() {
    tokio_xmpp::rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    gpui_kit::application()
        .with_assets(AppAssets)
        // Hide-to-tray closes the window (Wayland): quit only explicitly, via the tray or the login close.
        .with_quit_mode(QuitMode::Explicit)
        .run(|cx| {
            gpui_kit::init(cx);
            app::open_main_window(cx).expect("failed to open window");
        });
}
