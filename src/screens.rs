//! Windows screen-share targets and ffmpeg capture arguments.

/// Something to share on Windows: a monitor or a top-level window.
#[derive(Clone, Debug, PartialEq)]
pub enum ScreenTarget {
    /// A monitor, by its rectangle on the virtual desktop (may start at negative coordinates).
    Monitor { name: String, x: i32, y: i32, width: u32, height: u32, primary: bool },
    /// A top-level window, matched by title the way gdigrab does.
    Window { title: String },
}

/// Monitors (primary first) then shareable windows. Empty off Windows.
pub fn list() -> Vec<ScreenTarget> {
    #[cfg(windows)]
    { imp::list() }
    #[cfg(not(windows))]
    { Vec::new() }
}

/// ffmpeg input options capturing a monitor or window with gdigrab at 15 fps.
pub fn gdigrab_input(t: &ScreenTarget) -> Vec<String> {
    let mut args: Vec<String> = ["-f", "gdigrab", "-framerate", "15"].map(String::from).into();
    match t {
        ScreenTarget::Monitor { x, y, width, height, .. } => {
            args.extend(["-offset_x".into(), x.to_string(), "-offset_y".into(), y.to_string(), "-video_size".into(), format!("{width}x{height}"), "-i".into(), "desktop".into()]);
        }
        ScreenTarget::Window { title } => args.extend(["-i".into(), format!("title={title}")]),
    }
    args
}

#[cfg(windows)]
mod imp {
    use windows::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
    use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFOEXW};
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, MONITORINFOF_PRIMARY, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW};
    use windows::core::BOOL;

    use super::ScreenTarget;

    unsafe extern "system" fn monitor(handle: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if unsafe { GetMonitorInfoW(handle, &mut info.monitorInfo) }.as_bool() {
            let len = info.szDevice.iter().position(|&c| c == 0).unwrap_or(info.szDevice.len());
            let name = String::from_utf16_lossy(&info.szDevice[..len]);
            let rect = info.monitorInfo.rcMonitor;
            let targets = unsafe { &mut *(data.0 as *mut Vec<ScreenTarget>) };
            targets.push(ScreenTarget::Monitor {
                name: name.strip_prefix(r"\\.\").unwrap_or(&name).to_string(),
                x: rect.left,
                y: rect.top,
                width: (rect.right - rect.left) as u32,
                height: (rect.bottom - rect.top) as u32,
                primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
            });
        }
        true.into()
    }

    unsafe extern "system" fn window(hwnd: HWND, data: LPARAM) -> BOOL {
        unsafe {
            if !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool() || GetWindow(hwnd, GW_OWNER).is_ok_and(|owner| !owner.0.is_null()) {
                return true.into();
            }
            let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
            if style & WS_EX_TOOLWINDOW.0 != 0 && style & WS_EX_APPWINDOW.0 == 0 {
                return true.into();
            }
            let mut pid = 0;
            if GetWindowThreadProcessId(hwnd, Some(&mut pid)) == 0 || pid == GetCurrentProcessId() {
                return true.into();
            }
            let mut cloaked = 0u32;
            if DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&mut cloaked as *mut u32).cast(), std::mem::size_of::<u32>() as u32).is_ok() && cloaked != 0 {
                return true.into();
            }
            let len = GetWindowTextLengthW(hwnd);
            if len <= 0 {
                return true.into();
            }
            let mut text = vec![0u16; len as usize + 1];
            let len = GetWindowTextW(hwnd, &mut text);
            if len <= 0 {
                return true.into();
            }
            let title = String::from_utf16_lossy(&text[..len as usize]);
            if title != "Program Manager" {
                let targets = &mut *(data.0 as *mut Vec<ScreenTarget>);
                let target = ScreenTarget::Window { title };
                if !targets.contains(&target) {
                    targets.push(target);
                }
            }
        }
        true.into()
    }

    pub fn list() -> Vec<ScreenTarget> {
        let mut targets = Vec::<ScreenTarget>::new();
        let data = LPARAM(&mut targets as *mut _ as isize);
        let _ = unsafe { EnumDisplayMonitors(None, None, Some(monitor), data) };
        targets.sort_by_key(|t| match t {
            ScreenTarget::Monitor { primary, x, .. } => (!primary, *x),
            ScreenTarget::Window { .. } => (true, 0),
        });
        let _ = unsafe { EnumWindows(Some(window), data) };
        targets
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gdigrab_arguments() {
        let monitor = ScreenTarget::Monitor { name: "DISPLAY2".into(), x: -1920, y: -120, width: 1920, height: 1080, primary: false };
        assert_eq!(gdigrab_input(&monitor), ["-f", "gdigrab", "-framerate", "15", "-offset_x", "-1920", "-offset_y", "-120", "-video_size", "1920x1080", "-i", "desktop"]);
        let window = ScreenTarget::Window { title: "Рабочее окно 🦀".into() };
        assert_eq!(gdigrab_input(&window), ["-f", "gdigrab", "-framerate", "15", "-i", "title=Рабочее окно 🦀"]);
    }
}
