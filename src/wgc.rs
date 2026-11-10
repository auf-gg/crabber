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
    use windows::Win32::Foundation::{HMODULE, HWND, POINT};
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Direct3D11::{D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D};
    use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
    use windows::Win32::Graphics::Dxgi::IDXGIDevice;
    use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONULL, MonitorFromPoint};
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
            // apps get it without a prompt); Windows 10 has neither call and always draws it.
            let _ = GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless).and_then(|op| op.join());
            let _ = capture.session.SetIsBorderRequired(false);
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
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe { texture.GetDesc(&mut desc); }
            let (width, height) = (size.Width as u32, size.Height as u32);
            if desc.Width < width || desc.Height < height || desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                return Err("invalid screen capture texture".into());
            }
            if self.staging.is_none() {
                desc.Usage = D3D11_USAGE_STAGING;
                desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                desc.BindFlags = 0;
                desc.MiscFlags = 0;
                unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut self.staging)) }
                    .map_err(|_| "could not create screen capture readback texture".to_string())?;
            }
            let staging = self.staging.as_ref().ok_or("screen capture readback texture is missing")?;
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            unsafe {
                self.context.CopyResource(staging, &texture);
                self.context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            }.map_err(|_| "could not map screen capture texture".to_string())?;
            let data = (|| {
                let pitch = mapped.RowPitch as usize;
                let len = pitch.checked_mul(height as usize).filter(|&len| len <= isize::MAX as usize)
                    .ok_or("invalid screen capture buffer")?;
                if mapped.pData.is_null() { return Err("invalid screen capture buffer".into()); }
                let source = unsafe { std::slice::from_raw_parts(mapped.pData.cast::<u8>(), len) };
                super::copy_rows(source, width, height, pitch)
            })();
            unsafe { self.context.Unmap(staging, 0); }
            Ok(Some(RawFrame { width, height, data: data? }))
        }

        fn run(&mut self, fps: u32, stop: &Receiver<()>, on_frame: &mut dyn FnMut(RawFrame)) -> Result<(), String> {
            let interval = Duration::from_secs_f64(1.0 / f64::from(fps));
            loop {
                let started = Instant::now();
                if !matches!(stop.try_recv(), Err(TryRecvError::Empty)) { return Ok(()); }
                if let Some(frame) = self.next_frame()? { on_frame(frame); }
                match stop.recv_timeout(interval.saturating_sub(started.elapsed())) {
                    Err(RecvTimeoutError::Timeout) => {},
                    _ => return Ok(()),
                }
            }
        }
    }

    /// Captures `target` at up to `fps` frames per second, delivering tightly packed BGRA on a capture thread.
    /// Fails before Windows 10 1903 or when the target is gone.
    pub fn start(target: &ScreenTarget, fps: u32, mut on_frame: impl FnMut(RawFrame) + Send + 'static) -> Result<Capture, String> {
        if fps == 0 { return Err("screen capture frame rate must be positive".into()); }
        let target = target.clone();
        let (stop, stopped) = mpsc::channel();
        let (ready, started) = mpsc::channel();
        let thread = thread::Builder::new().name("screen-capture".into()).spawn(move || {
            let mut run = || -> Result<(), String> {
                unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.map_err(|_| "could not initialize screen capture runtime".to_string())?;
                let _runtime = Runtime;
                let mut session = Session::new(&target)?;
                if ready.send(Ok(())).is_ok() { session.run(fps, &stopped, &mut on_frame)?; }
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
