//! Windows screen-share targets and ffmpeg capture arguments.

/// Something to share on Windows: a monitor or a top-level window.
#[derive(Clone, Debug, PartialEq)]
pub enum ScreenTarget {
    /// A monitor, by its rectangle on the virtual desktop (may start at negative coordinates).
    Monitor { name: String, x: i32, y: i32, width: u32, height: u32, primary: bool },
    /// A top-level window, matched by title the way gdigrab does.
    Window { title: String, hwnd: isize },
}

/// Monitors (primary first) then shareable windows. Empty off Windows.
pub fn list() -> Vec<ScreenTarget> {
    #[cfg(windows)]
    { imp::list() }
    #[cfg(not(windows))]
    { Vec::new() }
}

/// A small picture of `t` for the share picker: tightly packed BGRA (what gpui's RenderImage takes),
/// at most `max_w`×`max_h`, aspect kept. None off Windows or when capture fails.
pub fn thumbnail(t: &ScreenTarget, max_w: u32, max_h: u32) -> Option<(u32, u32, Vec<u8>)> {
    #[cfg(windows)]
    { imp::thumbnail(t, max_w, max_h) }
    #[cfg(not(windows))]
    { let _ = (t, max_w, max_h); None }
}

#[cfg(any(windows, test))]
fn thumbnail_size(width: u32, height: u32, max_w: u32, max_h: u32) -> Option<(u32, u32)> {
    if width == 0 || height == 0 || max_w == 0 || max_h == 0 {
        return None;
    }
    if u64::from(width) * u64::from(max_h) > u64::from(height) * u64::from(max_w) {
        let w = width.min(max_w);
        Some((w, (u64::from(height) * u64::from(w) / u64::from(width)).max(1) as u32))
    } else {
        let h = height.min(max_h);
        Some(((u64::from(width) * u64::from(h) / u64::from(height)).max(1) as u32, h))
    }
}

/// ffmpeg input options capturing a monitor or window with gdigrab at 15 fps.
pub fn gdigrab_input(t: &ScreenTarget) -> Vec<String> {
    let mut args: Vec<String> = ["-f", "gdigrab", "-framerate", "15"].map(String::from).into();
    match t {
        ScreenTarget::Monitor { x, y, width, height, .. } => {
            args.extend(["-offset_x".into(), x.to_string(), "-offset_y".into(), y.to_string(), "-video_size".into(), format!("{width}x{height}"), "-i".into(), "desktop".into()]);
        }
        ScreenTarget::Window { title, .. } => args.extend(["-i".into(), format!("title={title}")]),
    }
    args
}

#[cfg(windows)]
mod imp {
    use windows::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
    use windows::Win32::Graphics::Gdi::{BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleBitmap, CreateCompatibleDC, DIB_RGB_COLORS, DeleteDC, DeleteObject, EnumDisplayMonitors, GetDC, GetDIBits, GetMonitorInfoW, HALFTONE, HBITMAP, HDC, HGDIOBJ, HMONITOR, MONITORINFOEXW, ReleaseDC, SRCCOPY, SelectObject, SetBrushOrgEx, SetStretchBltMode, StretchBlt};
    use windows::Win32::Storage::Xps::{PRINT_WINDOW_FLAGS, PrintWindow};
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindowVisible, MONITORINFOF_PRIMARY, WS_EX_APPWINDOW, WS_EX_TOOLWINDOW};
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
                let target = ScreenTarget::Window { title, hwnd: hwnd.0 as isize };
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

    struct ScreenDc(HDC);

    impl Drop for ScreenDc {
        fn drop(&mut self) {
            unsafe { ReleaseDC(None, self.0); }
        }
    }

    struct BitmapDc { dc: HDC, bitmap: HBITMAP, previous: HGDIOBJ }

    impl BitmapDc {
        fn new(screen: HDC, width: i32, height: i32) -> Option<Self> {
            unsafe {
                let dc = CreateCompatibleDC(Some(screen));
                if dc.0.is_null() { return None; }
                let bitmap = CreateCompatibleBitmap(screen, width, height);
                if bitmap.0.is_null() {
                    let _ = DeleteDC(dc);
                    return None;
                }
                let previous = SelectObject(dc, HGDIOBJ(bitmap.0));
                if previous.0.is_null() || previous.0 as isize == -1 {
                    let _ = DeleteObject(HGDIOBJ(bitmap.0));
                    let _ = DeleteDC(dc);
                    return None;
                }
                Some(Self { dc, bitmap, previous })
            }
        }
    }

    impl Drop for BitmapDc {
        fn drop(&mut self) {
            unsafe {
                SelectObject(self.dc, self.previous);
                let _ = DeleteObject(HGDIOBJ(self.bitmap.0));
                let _ = DeleteDC(self.dc);
            }
        }
    }

    pub fn thumbnail(t: &ScreenTarget, max_w: u32, max_h: u32) -> Option<(u32, u32, Vec<u8>)> {
        unsafe {
            let (x, y, width, height) = match t {
                ScreenTarget::Monitor { x, y, width, height, .. } => (*x, *y, i32::try_from(*width).ok()?, i32::try_from(*height).ok()?),
                ScreenTarget::Window { hwnd, .. } => {
                    let mut rect = RECT::default();
                    GetWindowRect(HWND(*hwnd as *mut _), &mut rect).ok()?;
                    (0, 0, rect.right.checked_sub(rect.left)?, rect.bottom.checked_sub(rect.top)?)
                }
            };
            let (w, h) = super::thumbnail_size(u32::try_from(width).ok()?, u32::try_from(height).ok()?, max_w, max_h)?;
            let screen = ScreenDc(GetDC(None));
            if screen.0.0.is_null() { return None; }
            let output = BitmapDc::new(screen.0, w as i32, h as i32)?;
            let window = if let ScreenTarget::Window { hwnd, .. } = t {
                let capture = BitmapDc::new(screen.0, width, height)?;
                const PW_RENDERFULLCONTENT: PRINT_WINDOW_FLAGS = PRINT_WINDOW_FLAGS(0x2);
                if !PrintWindow(HWND(*hwnd as *mut _), capture.dc, PW_RENDERFULLCONTENT).as_bool() { return None; }
                Some(capture)
            } else { None };
            if SetStretchBltMode(output.dc, HALFTONE) == 0 { return None; }
            if !SetBrushOrgEx(output.dc, 0, 0, None).as_bool() { return None; }
            let source = window.as_ref().map_or(screen.0, |capture| capture.dc);
            if !StretchBlt(output.dc, 0, 0, w as i32, h as i32, Some(source), x, y, width, height, SRCCOPY).as_bool() { return None; }
            // GetDIBits requires that the bitmap is not selected into a DC.
            SelectObject(output.dc, output.previous);
            let mut info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w as i32,
                    biHeight: -(h as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let len = (w as usize).checked_mul(h as usize)?.checked_mul(4)?;
            let mut pixels = Vec::new();
            pixels.try_reserve_exact(len).ok()?;
            pixels.resize(len, 0);
            if GetDIBits(screen.0, output.bitmap, 0, h, Some(pixels.as_mut_ptr().cast()), &mut info, DIB_RGB_COLORS) != h as i32 { return None; }
            for pixel in pixels.chunks_exact_mut(4) { pixel[3] = 255; }
            Some((w, h, pixels))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gdigrab_arguments() {
        let monitor = ScreenTarget::Monitor { name: "DISPLAY2".into(), x: -1920, y: -120, width: 1920, height: 1080, primary: false };
        assert_eq!(gdigrab_input(&monitor), ["-f", "gdigrab", "-framerate", "15", "-offset_x", "-1920", "-offset_y", "-120", "-video_size", "1920x1080", "-i", "desktop"]);
        let window = ScreenTarget::Window { title: "Рабочее окно 🦀".into(), hwnd: 42 };
        assert_eq!(gdigrab_input(&window), ["-f", "gdigrab", "-framerate", "15", "-i", "title=Рабочее окно 🦀"]);
    }

    #[test]
    fn thumbnail_dimensions() {
        assert_eq!(thumbnail_size(1920, 1080, 320, 240), Some((320, 180)));
        assert_eq!(thumbnail_size(1080, 1920, 320, 240), Some((135, 240)));
        assert_eq!(thumbnail_size(80, 40, 320, 240), Some((80, 40)));
        assert_eq!(thumbnail_size(0, 1080, 320, 240), None);
        assert_eq!(thumbnail_size(1920, 1080, 0, 240), None);
        assert_eq!(thumbnail_size(1, 1000, 100, 1), Some((1, 1)));
        assert_eq!(thumbnail_size(u32::MAX, u32::MAX, 320, 240), Some((240, 240)));
    }

    #[cfg(not(windows))]
    #[test]
    fn thumbnails_unavailable_off_windows() {
        assert_eq!(thumbnail(&ScreenTarget::Window { title: "test".into(), hwnd: 0 }, 320, 240), None);
    }
}
