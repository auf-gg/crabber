//! GPU screen capture with a tightly packed CPU copy for the video encoder.

#[cfg(windows)]
pub use imp::{Capture, start};

fn copy_rows(source: &[u8], width: u32, height: u32, pitch: usize) -> Result<Vec<u8>, String> {
    let invalid = || "invalid screen capture buffer".to_string();
    let row_bytes = (width as usize).checked_mul(4).ok_or_else(invalid)?;
    let len = row_bytes.checked_mul(height as usize).ok_or_else(invalid)?;
    let span = pitch.checked_mul(height as usize).ok_or_else(invalid)?;
    if width == 0 || height == 0 || pitch < row_bytes || source.len() < span {
        return Err(invalid());
    }
    let mut pixels = Vec::new();
    pixels.try_reserve_exact(len).map_err(|_| "not enough memory for screen capture".to_string())?;
    for row in source[..span].chunks_exact(pitch) {
        pixels.extend_from_slice(&row[..row_bytes]);
    }
    Ok(pixels)
}

#[cfg(windows)]
mod imp {
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
    use std::thread::{self, JoinHandle};
    use std::time::{Duration, Instant};
    use windows::core::{Interface, factory};
    use windows::Graphics::Capture::{Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureAccess, GraphicsCaptureAccessKind, GraphicsCaptureItem, GraphicsCaptureSession};
    use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
    use windows::Graphics::DirectX::DirectXPixelFormat;
    use windows::Graphics::SizeInt32;
    use windows::Win32::Foundation::{E_ACCESSDENIED, HMODULE, HWND, POINT};
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_UNKNOWN};
    use windows::Win32::Graphics::Direct3D11::{D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter1, IDXGIDevice, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource};
    use windows::Win32::Graphics::Gdi::{BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS, DeleteDC, DeleteObject, GdiFlush, HGDIOBJ, HMONITOR, MONITOR_DEFAULTTONULL, MonitorFromPoint, SelectObject};
    use windows::Win32::UI::WindowsAndMessaging::{CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, GetIconInfo, HICON, ICONINFO};
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};
    use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
    use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

    use crate::screens::ScreenTarget;
    use crate::videocall::RawFrame;

    const FORMAT: DirectXPixelFormat = DirectXPixelFormat::B8G8R8A8UIntNormalized;

    /// Running capture; dropping it stops it.
    pub struct Capture { stop: Sender<()>, thread: Option<JoinHandle<()>> }

    impl Drop for Capture {
        fn drop(&mut self) {
            let _ = self.stop.send(());
            if let Some(thread) = self.thread.take()
                && thread.thread().id() != thread::current().id() {
                let _ = thread.join();
            }
        }
    }

    struct Runtime;
    impl Drop for Runtime {
        fn drop(&mut self) { unsafe { RoUninitialize(); } }
    }

    struct Frame(Direct3D11CaptureFrame);
    impl Drop for Frame {
        fn drop(&mut self) { let _ = self.0.Close(); }
    }

    struct Session {
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        winrt_device: IDirect3DDevice,
        pool: Direct3D11CaptureFramePool,
        session: GraphicsCaptureSession,
        size: SizeInt32,
        staging: Option<ID3D11Texture2D>,
    }

    impl Drop for Session {
        fn drop(&mut self) {
            let _ = self.session.Close();
            let _ = self.pool.Close();
        }
    }

    impl Session {
        fn new(target: &ScreenTarget) -> Result<Self, String> {
            if !GraphicsCaptureSession::IsSupported().unwrap_or(false) {
                return Err("Windows screen capture is unavailable".into());
            }
            let interop: IGraphicsCaptureItemInterop = factory::<GraphicsCaptureItem, _>()
                .map_err(|_| "Windows screen capture requires Windows 10 1903 or newer".to_string())?;
            let gone = || "screen share target is gone".to_string();
            let item: GraphicsCaptureItem = unsafe {
                match target {
                    ScreenTarget::Window { hwnd, .. } => interop.CreateForWindow(HWND(*hwnd as *mut _)),
                    ScreenTarget::Monitor { x, y, width, height, .. } => {
                        if *width == 0 || *height == 0 { return Err(gone()); }
                        let point = POINT {
                            x: i32::try_from(i64::from(*x) + i64::from(*width) / 2).map_err(|_| gone())?,
                            y: i32::try_from(i64::from(*y) + i64::from(*height) / 2).map_err(|_| gone())?,
                        };
                        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONULL);
                        if monitor.0.is_null() { return Err(gone()); }
                        interop.CreateForMonitor(monitor)
                    }
                }
            }.map_err(|_| gone())?;
            let size = item.Size().map_err(|_| gone())?;
            if size.Width <= 0 || size.Height <= 0 { return Err(gone()); }
            let (mut device, mut context) = (None, None);
            unsafe {
                D3D11CreateDevice(None, D3D_DRIVER_TYPE_HARDWARE, HMODULE::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None, D3D11_SDK_VERSION, Some(&mut device), None, Some(&mut context))
            }.map_err(|_| "could not create screen capture graphics device".to_string())?;
            let device = device.ok_or("screen capture graphics device is missing")?;
            let context = context.ok_or("screen capture graphics context is missing")?;
            let dxgi: IDXGIDevice = device.cast().map_err(|_| "screen capture graphics device is unsupported".to_string())?;
            let winrt_device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
                .and_then(|device| device.cast()).map_err(|_| "could not wrap screen capture graphics device".to_string())?;
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&winrt_device, FORMAT, 2, size)
                .map_err(|_| "could not create screen capture frame pool".to_string())?;
            let session = match pool.CreateCaptureSession(&item) {
                Ok(session) => session,
                Err(_) => {
                    let _ = pool.Close();
                    return Err("could not create screen capture session".into());
                }
            };
            let capture = Self { device, context, winrt_device, pool, session, size, staging: None };
            let _ = capture.session.SetIsCursorCaptureEnabled(true);
            // Windows 11 drops the yellow border only once borderless capture is granted (desktop
            // apps get it without a prompt); Windows 10 has neither call and always draws it, so
            // there we give up and let the caller pick a capture method without a border.
            let _ = GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless).and_then(|op| op.join());
            if capture.session.SetIsBorderRequired(false).is_err() {
                return Err(BORDER.into());
            }
            capture.session.StartCapture().map_err(|_| "could not start screen capture".to_string())?;
            Ok(capture)
        }

        fn next_frame(&mut self) -> Result<Option<RawFrame>, String> {
            let mut latest = None;
            for _ in 0..2 {
                match self.pool.TryGetNextFrame() {
                    Ok(frame) => latest = Some(Frame(frame)),
                    // A null WinRT result is represented as an error with S_OK.
                    Err(error) if error.code().is_ok() => break,
                    Err(_) => return Err("screen capture stopped".into()),
                }
            }
            let Some(frame) = latest else { return Ok(None); };
            let size = frame.0.ContentSize().map_err(|_| "could not read screen capture size".to_string())?;
            if size.Width <= 0 || size.Height <= 0 { return Ok(None); }
            if size != self.size {
                drop(frame);
                self.pool.Recreate(&self.winrt_device, FORMAT, 2, size).map_err(|_| "could not resize screen capture".to_string())?;
                self.size = size;
                self.staging = None;
                return Ok(None);
            }
            let access: IDirect3DDxgiInterfaceAccess = frame.0.Surface().and_then(|surface| surface.cast())
                .map_err(|_| "could not read screen capture surface".to_string())?;
            let texture: ID3D11Texture2D = unsafe { access.GetInterface() }
                .map_err(|_| "could not read screen capture texture".to_string())?;
            let (width, height) = (size.Width as u32, size.Height as u32);
            read_texture(&self.device, &self.context, &mut self.staging, &texture, width, height).map(Some)
        }

    }

    /// Error of `Session::new` when Windows would draw a yellow border around the capture.
    const BORDER: &str = "screen capture border cannot be hidden";

    /// Copies a BGRA texture into a tightly packed buffer through a CPU-readable staging texture.
    fn read_texture(device: &ID3D11Device, context: &ID3D11DeviceContext, staging: &mut Option<ID3D11Texture2D>,
        texture: &ID3D11Texture2D, width: u32, height: u32) -> Result<RawFrame, String> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc); }
        if desc.Width < width || desc.Height < height || desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err("invalid screen capture texture".into());
        }
        if let Some(current) = staging.as_ref() {
            let mut old = D3D11_TEXTURE2D_DESC::default();
            unsafe { current.GetDesc(&mut old); }
            if old.Width != desc.Width || old.Height != desc.Height { *staging = None; }
        }
        if staging.is_none() {
            desc.Usage = D3D11_USAGE_STAGING;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            desc.BindFlags = 0;
            desc.MiscFlags = 0;
            unsafe { device.CreateTexture2D(&desc, None, Some(staging)) }
                .map_err(|_| "could not create screen capture readback texture".to_string())?;
        }
        let staging = staging.as_ref().ok_or("screen capture readback texture is missing")?;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            context.CopyResource(staging, texture);
            context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
        }.map_err(|_| "could not map screen capture texture".to_string())?;
        let data = (|| {
            let pitch = mapped.RowPitch as usize;
            let len = pitch.checked_mul(height as usize).filter(|&len| len <= isize::MAX as usize)
                .ok_or("invalid screen capture buffer")?;
            if mapped.pData.is_null() { return Err("invalid screen capture buffer".into()); }
            let source = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), len) };
            super::copy_rows(source, width, height, pitch)
        })();
        unsafe { context.Unmap(staging, 0); }
        Ok(RawFrame { width, height, data: data? })
    }

    /// Monitor capture through DXGI Desktop Duplication: no yellow border on Windows 10, but no
    /// cursor in the picture either, so we draw it ourselves.
    struct Duplication {
        device: ID3D11Device,
        context: ID3D11DeviceContext,
        output: IDXGIOutput1,
        origin: (i32, i32),
        duplication: Option<IDXGIOutputDuplication>,
        staging: Option<ID3D11Texture2D>,
        /// Last desktop picture, without the cursor.
        desktop: Option<RawFrame>,
        cursor: Option<(isize, i32, i32)>,
        lost_since: Option<Instant>,
    }

    impl Duplication {
        fn new(target: &ScreenTarget) -> Result<Self, String> {
            let gone = || "screen share target is gone".to_string();
            let ScreenTarget::Monitor { x, y, width, height, .. } = target else {
                return Err("desktop duplication captures monitors only".into());
            };
            if *width == 0 || *height == 0 { return Err(gone()); }
            let point = POINT {
                x: i32::try_from(i64::from(*x) + i64::from(*width) / 2).map_err(|_| gone())?,
                y: i32::try_from(i64::from(*y) + i64::from(*height) / 2).map_err(|_| gone())?,
            };
            let monitor = unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONULL) };
            if monitor.0.is_null() { return Err(gone()); }
            let (adapter, output, origin) = find_output(monitor)?;
            let (mut device, mut context) = (None, None);
            unsafe {
                D3D11CreateDevice(&adapter, D3D_DRIVER_TYPE_UNKNOWN, HMODULE::default(), D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None, D3D11_SDK_VERSION, Some(&mut device), None, Some(&mut context))
            }.map_err(|_| "could not create screen capture graphics device".to_string())?;
            let device: ID3D11Device = device.ok_or("screen capture graphics device is missing")?;
            let context = context.ok_or("screen capture graphics context is missing")?;
            let duplication = unsafe { output.DuplicateOutput(&device) }
                .map_err(|_| "could not start desktop duplication".to_string())?;
            Ok(Self { device, context, output, origin, duplication: Some(duplication), staging: None,
                desktop: None, cursor: None, lost_since: None })
        }

        fn next_frame(&mut self) -> Result<Option<RawFrame>, String> {
            let mut changed = self.update_desktop()?;
            let cursor = cursor(self.origin);
            if cursor.map(|(handle, x, y)| (handle.0 as isize, x, y)) != self.cursor {
                self.cursor = cursor.map(|(handle, x, y)| (handle.0 as isize, x, y));
                changed = true;
            }
            let Some(desktop) = self.desktop.as_ref().filter(|_| changed) else { return Ok(None); };
            let mut frame = RawFrame { width: desktop.width, height: desktop.height, data: desktop.data.clone() };
            if let Some((handle, x, y)) = cursor { draw_cursor(&mut frame, handle, x, y); }
            Ok(Some(frame))
        }

        /// Refreshes the desktop picture; true when it changed.
        fn update_desktop(&mut self) -> Result<bool, String> {
            if self.duplication.is_none() {
                // The duplication is lost on resolution changes and while the secure desktop
                // (UAC, lock screen) is up; keep trying for a while before giving up.
                match unsafe { self.output.DuplicateOutput(&self.device) } {
                    Ok(duplication) => { self.duplication = Some(duplication); self.lost_since = None; }
                    Err(_) if self.lost_since.get_or_insert_with(Instant::now).elapsed() < LOST_TIMEOUT => return Ok(false),
                    Err(_) => return Err("screen capture stopped".into()),
                }
            }
            let Some(duplication) = self.duplication.clone() else { return Ok(false); };
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            match unsafe { duplication.AcquireNextFrame(0, &mut info, &mut resource) } {
                Ok(()) => {},
                Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
                Err(error) if error.code() == DXGI_ERROR_ACCESS_LOST || error.code() == E_ACCESSDENIED => {
                    self.duplication = None;
                    return Ok(false);
                }
                Err(_) => return Err("screen capture stopped".into()),
            }
            // Frames with only cursor updates (and the very first one) carry no fresh picture.
            let result = if info.LastPresentTime != 0 {
                (|| {
                    let texture: ID3D11Texture2D = resource.ok_or("screen capture frame is missing")?.cast()
                        .map_err(|_| "could not read screen capture texture".to_string())?;
                    let mut desc = D3D11_TEXTURE2D_DESC::default();
                    unsafe { texture.GetDesc(&mut desc); }
                    self.desktop = Some(read_texture(&self.device, &self.context, &mut self.staging, &texture, desc.Width, desc.Height)?);
                    Ok::<_, String>(true)
                })()
            } else { Ok(false) };
            let _ = unsafe { duplication.ReleaseFrame() };
            result
        }
    }

    const LOST_TIMEOUT: Duration = Duration::from_secs(10);

    /// The DXGI adapter and output showing `monitor`, and the output's top-left corner.
    fn find_output(monitor: HMONITOR) -> Result<(IDXGIAdapter1, IDXGIOutput1, (i32, i32)), String> {
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(|_| "could not list graphics adapters".to_string())?;
        for a in 0.. {
            let Ok(adapter) = (unsafe { factory.EnumAdapters1(a) }) else { break };
            for o in 0.. {
                let Ok(output) = (unsafe { adapter.EnumOutputs(o) }) else { break };
                let Ok(desc) = (unsafe { output.GetDesc() }) else { continue };
                if desc.Monitor == monitor {
                    let output: IDXGIOutput1 = output.cast().map_err(|_| "desktop duplication is unsupported".to_string())?;
                    let rect = desc.DesktopCoordinates;
                    return Ok((adapter, output, (rect.left, rect.top)));
                }
            }
        }
        Err("screen share target is gone".into())
    }

    /// The visible cursor and its position relative to `origin`.
    fn cursor(origin: (i32, i32)) -> Option<(HICON, i32, i32)> {
        let mut info = CURSORINFO { cbSize: std::mem::size_of::<CURSORINFO>() as u32, ..Default::default() };
        unsafe { GetCursorInfo(&mut info) }.ok()?;
        if info.flags.0 & CURSOR_SHOWING.0 == 0 || info.hCursor.0.is_null() { return None; }
        Some((HICON(info.hCursor.0), info.ptScreenPos.x - origin.0, info.ptScreenPos.y - origin.1))
    }

    /// Draws the cursor with GDI onto the part of `frame` it covers; `x`, `y` is its hot spot.
    fn draw_cursor(frame: &mut RawFrame, cursor: HICON, x: i32, y: i32) {
        const SIZE: i32 = 256;
        unsafe {
            let mut icon = ICONINFO::default();
            if GetIconInfo(cursor, &mut icon).is_err() { return; }
            for bitmap in [icon.hbmMask, icon.hbmColor] {
                if !bitmap.0.is_null() { let _ = DeleteObject(HGDIOBJ(bitmap.0)); }
            }
            let (left, top) = (x - icon.xHotspot as i32, y - icon.yHotspot as i32);
            let (Ok(width), Ok(height)) = (i32::try_from(frame.width), i32::try_from(frame.height)) else { return };
            let (x0, y0) = (left.max(0), top.max(0));
            let (x1, y1) = (left.saturating_add(SIZE).min(width), top.saturating_add(SIZE).min(height));
            if x0 >= x1 || y0 >= y1 { return; }
            let (w, h) = (x1 - x0, y1 - y0);
            let info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w,
                    biHeight: -h,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let Ok(bitmap) = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) else { return };
            let dc = CreateCompatibleDC(None);
            if !dc.0.is_null() && !bits.is_null() {
                let previous = SelectObject(dc, HGDIOBJ(bitmap.0));
                let pixels = std::slice::from_raw_parts_mut(bits.cast::<u8>(), (w * h * 4) as usize);
                let (row, stride) = (w as usize * 4, frame.width as usize * 4);
                let at = |line: i32| (y0 + line) as usize * stride + x0 as usize * 4;
                for line in 0..h {
                    let start = at(line);
                    pixels[line as usize * row..][..row].copy_from_slice(&frame.data[start..start + row]);
                }
                let drawn = DrawIconEx(dc, left - x0, top - y0, cursor, 0, 0, 0, None, DI_NORMAL).is_ok();
                let _ = GdiFlush();
                if drawn {
                    for line in 0..h {
                        let start = at(line);
                        let target = &mut frame.data[start..start + row];
                        target.copy_from_slice(&pixels[line as usize * row..][..row]);
                        for pixel in target.chunks_exact_mut(4) { pixel[3] = 255; }
                    }
                }
                SelectObject(dc, previous);
            }
            if !dc.0.is_null() { let _ = DeleteDC(dc); }
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
        }
    }

    fn pump(fps: u32, stop: &Receiver<()>, on_frame: &mut dyn FnMut(RawFrame),
        mut next_frame: impl FnMut() -> Result<Option<RawFrame>, String>) -> Result<(), String> {
        let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
        loop {
            let started = Instant::now();
            if !matches!(stop.try_recv(), Err(TryRecvError::Empty)) { return Ok(()); }
            if let Some(frame) = next_frame()? { on_frame(frame); }
            match stop.recv_timeout(interval.saturating_sub(started.elapsed())) {
                Err(RecvTimeoutError::Timeout) => {},
                _ => return Ok(()),
            }
        }
    }

    /// Captures `target` at up to `fps` frames per second, delivering tightly packed BGRA on a capture thread.
    /// Monitors use Windows Graphics Capture where its yellow border can be hidden (Windows 11) and
    /// Desktop Duplication otherwise. Fails before Windows 10 1903, for windows on Windows 10 (the
    /// caller falls back to GDI, which has no border), or when the target is gone.
    pub fn start(target: &ScreenTarget, fps: u32, mut on_frame: impl FnMut(RawFrame) + Send + 'static) -> Result<Capture, String> {
        if fps == 0 { return Err("screen capture frame rate must be positive".into()); }
        let target = target.clone();
        let (stop, stopped) = mpsc::channel();
        let (ready, started) = mpsc::channel();
        let thread = thread::Builder::new().name("screen-capture".into()).spawn(move || {
            let mut run = || -> Result<(), String> {
                unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.map_err(|_| "could not initialize screen capture runtime".to_string())?;
                let _runtime = Runtime;
                match Session::new(&target) {
                    Ok(mut session) => {
                        if ready.send(Ok(())).is_ok() { pump(fps, &stopped, &mut on_frame, || session.next_frame())?; }
                    }
                    Err(error) if error == BORDER && matches!(target, ScreenTarget::Monitor { .. }) => {
                        let mut duplication = Duplication::new(&target)?;
                        if ready.send(Ok(())).is_ok() { pump(fps, &stopped, &mut on_frame, || duplication.next_frame())?; }
                    }
                    Err(error) => return Err(error),
                }
                Ok(())
            };
            if let Err(error) = run()
                && ready.send(Err(error.clone())).is_err() {
                eprintln!("screen capture: {error}");
            }
        }).map_err(|_| "could not start screen capture thread".to_string())?;
        let capture = Capture { stop, thread: Some(thread) };
        started.recv().map_err(|_| "screen capture thread stopped".to_string())??;
        Ok(capture)
    }
}

#[cfg(test)]
mod tests {
    use super::copy_rows;

    #[test]
    fn bgra_rows_are_tight_and_keep_odd_dimensions() {
        let source: Vec<u8> = (0..32).collect();
        assert_eq!(copy_rows(&source, 3, 2, 16).expect("valid rows"), [&source[..12], &source[16..28]].concat());
        assert!(copy_rows(&source, 3, 2, 8).is_err());
        assert!(copy_rows(&source[..20], 3, 2, 16).is_err());
        assert!(copy_rows(&source, 0, 2, 16).is_err());
    }
}
