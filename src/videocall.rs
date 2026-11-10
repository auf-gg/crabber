//! Video for calls through the system ffmpeg: camera/screen capture encoded to VP8, VP8 decoded to BGRA.
//! Frames travel as IVF (a 32-byte file header, then a 12-byte header per frame).
use crate::video::cmd;
use std::io::{Read, Write};
use std::process::{Child, Stdio};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Source {
    Camera,
    Screen,
}

/// Chosen camera (a /dev/videoN path, or a DirectShow name on Windows); None = the first one.
static CAMERA: Mutex<Option<String>> = Mutex::new(None);

pub fn set_camera(id: Option<String>) {
    *CAMERA.lock().unwrap() = id;
}

/// Cameras to choose from; the chosen one if still plugged in, else the first, is what calls use.
pub fn cameras() -> Vec<crate::audio::MediaDevice> {
    use crate::audio::MediaDevice;
    #[cfg(target_os = "linux")]
    {
        // Each camera also has metadata nodes; only index 0 captures video.
        let mut out: Vec<(u32, MediaDevice)> = std::fs::read_dir("/sys/class/video4linux")
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let node = e.file_name().to_string_lossy().to_string();
                let n: u32 = node.strip_prefix("video")?.parse().ok()?;
                let read = |f: &str| std::fs::read_to_string(e.path().join(f)).map(|s| s.trim().to_string());
                (read("index").ok()? == "0").then_some(())?;
                Some((n, MediaDevice { id: format!("/dev/{node}"), name: read("name").unwrap_or(node) }))
            })
            .collect();
        out.sort_by_key(|(n, _)| *n);
        out.into_iter().map(|(_, d)| d).collect()
    }
    #[cfg(windows)]
    {
        dshow_cameras().into_iter().map(|n| MediaDevice { id: n.clone(), name: n }).collect()
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        vec![]
    }
}

/// What a Windows screen share captures, picked in our own dialog (there is no system picker);
/// None: the whole desktop.
static SCREEN: Mutex<Option<crate::screens::ScreenTarget>> = Mutex::new(None);

pub fn set_screen(target: Option<crate::screens::ScreenTarget>) {
    *SCREEN.lock().unwrap() = target;
}

fn camera() -> Option<String> {
    let all = cameras();
    let chosen = CAMERA.lock().unwrap().clone();
    chosen.filter(|c| all.iter().any(|d| &d.id == c)).or_else(|| all.into_iter().next().map(|d| d.id))
}

/// Size of the decoded remote picture (letterboxed) and of the local preview.
pub const REMOTE: (u32, u32) = (1280, 720);
pub const PREVIEW: (u32, u32) = (320, 180);

/// Newest decoded frame of one side (taken by the UI) and when the last one arrived.
#[derive(Default)]
pub struct Slot {
    pub frame: Option<Vec<u8>>,
    pub at: Option<Instant>,
}

#[derive(Default)]
pub struct Frames {
    pub remote: Mutex<Slot>,
    pub local: Mutex<Slot>,
}

/// One call at a time, so one shared pair of pictures for the UI to poll.
pub static FRAMES: LazyLock<Frames> = LazyLock::new(Frames::default);

impl Frames {
    pub fn clear(&self) {
        *self.remote.lock().unwrap() = Slot::default();
        *self.local.lock().unwrap() = Slot::default();
    }
}

/// Receives each encoded VP8 frame.
pub(crate) type Sink = Arc<Mutex<Box<dyn FnMut(Vec<u8>) + Send>>>;

/// Running capture + encoder; dropping it stops ffmpeg (and the screencast).
pub struct Encoder {
    child: Arc<Mutex<Option<Child>>>,
    #[cfg(target_os = "linux")]
    _cast: Option<crate::screencast::Screencast>,
    _feed: Option<SyncSender<()>>,
}

pub(crate) fn encode_args(src: Source) -> Vec<String> {
    let (width, rate) = match src {
        Source::Camera => (1280, "1200k"),
        Source::Screen => (1920, "2000k"),
    };
    #[rustfmt::skip]
    let args = [
        "-an", "-vf", &format!("scale='min({width},iw)':-2,format=yuv420p"),
        "-c:v", "libvpx", "-deadline", "realtime", "-cpu-used", "8", "-lag-in-frames", "0", "-auto-alt-ref", "0",
        "-error-resilient", "1", "-g", "60", "-b:v", rate, "-maxrate", rate, "-bufsize", rate, "-threads", "4",
        "-f", "ivf", "-",
    ];
    args.iter().map(|s| s.to_string()).collect()
}

/// ffmpeg input options capturing `src` directly; None when frames must be fed through stdin (Wayland).
fn capture_args(src: Source) -> Result<Option<Vec<String>>, String> {
    let v = |a: &[&str]| Ok(Some(a.iter().map(|s| s.to_string()).collect()));
    match src {
        #[cfg(target_os = "linux")]
        Source::Camera => {
            let dev = camera().ok_or(crate::t!("call.no_camera"))?;
            v(&["-f", "v4l2", "-i", &dev])
        }
        #[cfg(target_os = "linux")]
        Source::Screen if crate::screencast::wayland() => Ok(None),
        #[cfg(target_os = "linux")]
        Source::Screen => v(&["-f", "x11grab", "-framerate", "15", "-i", &std::env::var("DISPLAY").unwrap_or(":0".into())]),
        #[cfg(windows)]
        Source::Camera => {
            let name = camera().ok_or(crate::t!("call.no_camera"))?;
            v(&["-f", "dshow", "-i", &format!("video={name}")])
        }
        #[cfg(windows)]
        Source::Screen => match SCREEN.lock().unwrap().clone() {
            Some(target) => Ok(Some(crate::screens::gdigrab_input(&target))),
            None => v(&["-f", "gdigrab", "-framerate", "15", "-i", "desktop"]),
        },
        #[cfg(not(any(target_os = "linux", windows)))]
        _ => Err(crate::t!("call.no_camera").into()),
    }
}

/// DirectShow video devices, from ffmpeg's device listing.
#[cfg(windows)]
fn dshow_cameras() -> Vec<String> {
    let Ok(out) = cmd("ffmpeg").args(["-hide_banner", "-list_devices", "true", "-f", "dshow", "-i", "dummy"]).stderr(Stdio::piped()).output() else { return vec![] };
    let text = String::from_utf8_lossy(&out.stderr);
    text.lines().filter(|l| l.contains("(video)")).filter_map(|l| Some(l.split('"').nth(1)?.to_string())).collect()
}

pub(crate) fn spawn_encoder(input: &[String], sink: Sink) -> Result<Child, String> {
    let mut child = cmd("ffmpeg")
        .args(["-v", "error", "-fflags", "nobuffer"])
        .args(input)
        .stdout(Stdio::piped())
        .stdin(if input.windows(2).any(|w| w[0] == "-i" && w[1] == "-") { Stdio::piped() } else { Stdio::null() })
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut out = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        let mut head = [0u8; 32];
        if out.read_exact(&mut head).is_err() {
            return;
        }
        let mut fh = [0u8; 12];
        while out.read_exact(&mut fh).is_ok() {
            let mut buf = vec![0u8; u32::from_le_bytes([fh[0], fh[1], fh[2], fh[3]]) as usize];
            if out.read_exact(&mut buf).is_err() {
                break;
            }
            (sink.lock().unwrap())(buf);
        }
    });
    Ok(child)
}

impl Encoder {
    /// Blocks while the screen picker is open (Wayland). `on_frame` gets each encoded VP8 frame.
    pub fn start(src: Source, on_frame: impl FnMut(Vec<u8>) + Send + 'static) -> Result<Self, String> {
        if !crate::video::available() {
            return Err(crate::t!("call.no_ffmpeg").into());
        }
        let sink: Sink = Arc::new(Mutex::new(Box::new(on_frame)));
        let encode = encode_args(src);
        match capture_args(src)? {
            Some(mut input) => {
                input.extend(encode);
                let child = spawn_encoder(&input, sink)?;
                Ok(Self {
                    child: Arc::new(Mutex::new(Some(child))),
                    #[cfg(target_os = "linux")]
                    _cast: None,
                    _feed: None,
                })
            }
            None => Self::start_fed(encode, sink),
        }
    }

    /// Wayland: screencast frames piped into ffmpeg as raw BGRA; ffmpeg restarts when the size changes.
    /// The last frame is repeated when the screen is static so a keyframe still comes every few seconds.
    #[cfg(target_os = "linux")]
    fn start_fed(encode: Vec<String>, sink: Sink) -> Result<Self, String> {
        let (tx, rx) = sync_channel::<crate::screencast::Frame>(2);
        let cast = crate::screencast::start(move |f| {
            let _ = tx.try_send(f);
        })?;
        let child = Arc::new(Mutex::new(None::<Child>));
        let (stop, stop_rx) = sync_channel::<()>(0);
        let held = child.clone();
        std::thread::spawn(move || {
            let mut last: Option<crate::screencast::Frame> = None;
            let mut size = (0, 0);
            loop {
                if !matches!(stop_rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)) {
                    break;
                }
                match rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(f) => last = Some(f),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(_) => break,
                }
                let Some(f) = last.as_ref() else { continue };
                let mut slot = held.lock().unwrap();
                if size != (f.width, f.height) || slot.is_none() {
                    if let Some(mut old) = slot.take() {
                        let _ = old.kill();
                        let _ = old.wait();
                    }
                    size = (f.width, f.height);
                    let mut input: Vec<String> = ["-f", "rawvideo", "-pix_fmt", "bgra", "-s", &format!("{}x{}", f.width, f.height), "-r", "15", "-i", "-"]
                        .iter()
                        .map(|s| s.to_string())
                        .collect();
                    input.extend(encode.iter().cloned());
                    match spawn_encoder(&input, sink.clone()) {
                        Ok(c) => *slot = Some(c),
                        Err(_) => break,
                    }
                }
                let Some(stdin) = slot.as_mut().and_then(|c| c.stdin.as_mut()) else { break };
                if stdin.write_all(&f.data).is_err() {
                    *slot = None;
                }
            }
        });
        Ok(Self { child, _cast: Some(cast), _feed: Some(stop) })
    }

    #[cfg(not(target_os = "linux"))]
    fn start_fed(_: Vec<String>, _: Sink) -> Result<Self, String> {
        unreachable!("only Wayland feeds frames")
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// VP8 decoder writing letterboxed BGRA frames of a fixed size into a slot; drop stops it.
pub struct Decoder {
    tx: Option<SyncSender<Vec<u8>>>,
    child: Child,
}

impl Decoder {
    pub fn start((w, h): (u32, u32), slot: &'static Mutex<Slot>) -> Result<Self, String> {
        if !crate::video::available() {
            return Err(crate::t!("call.no_ffmpeg").into());
        }
        let vf = format!("scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2");
        #[rustfmt::skip]
        let mut child = cmd("ffmpeg")
            .args(["-v", "error", "-fflags", "nobuffer", "-flags", "low_delay", "-probesize", "32", "-analyzeduration", "0",
                "-f", "ivf", "-i", "-", "-vf", &vf, "-fps_mode", "passthrough", "-f", "rawvideo", "-pix_fmt", "bgra", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        let mut stdin = child.stdin.take().unwrap();
        let mut out = child.stdout.take().unwrap();
        let (tx, rx) = sync_channel::<Vec<u8>>(8);
        std::thread::spawn(move || {
            // VP8 fourcc, 640x480 (the decoder takes the real size from the bitstream), 1/30 time base.
            let mut head = Vec::with_capacity(32);
            head.extend_from_slice(b"DKIF");
            head.extend_from_slice(&0u16.to_le_bytes());
            head.extend_from_slice(&32u16.to_le_bytes());
            head.extend_from_slice(b"VP80");
            head.extend_from_slice(&640u16.to_le_bytes());
            head.extend_from_slice(&480u16.to_le_bytes());
            head.extend_from_slice(&30u32.to_le_bytes());
            head.extend_from_slice(&1u32.to_le_bytes());
            head.extend_from_slice(&0u32.to_le_bytes());
            head.extend_from_slice(&0u32.to_le_bytes());
            if stdin.write_all(&head).is_err() {
                return;
            }
            for (pts, frame) in rx.into_iter().enumerate() {
                let mut fh = (frame.len() as u32).to_le_bytes().to_vec();
                fh.extend_from_slice(&(pts as u64).to_le_bytes());
                if stdin.write_all(&fh).and_then(|_| stdin.write_all(&frame)).and_then(|_| stdin.flush()).is_err() {
                    break;
                }
            }
        });
        std::thread::spawn(move || {
            loop {
                let mut buf = vec![0u8; (w * h * 4) as usize];
                if out.read_exact(&mut buf).is_err() {
                    break;
                }
                let mut s = slot.lock().unwrap();
                *s = Slot { frame: Some(buf), at: Some(Instant::now()) };
            }
        });
        Ok(Self { tx: Some(tx), child })
    }

    /// Queues one whole VP8 frame; dropped when the decoder lags (the caller then waits for a keyframe).
    pub fn push(&self, frame: Vec<u8>) -> bool {
        self.tx.as_ref().is_some_and(|tx| tx.try_send(frame).is_ok())
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        self.tx = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// VP8 keyframe: bit 0 of the first byte of the frame tag is 0.
pub fn is_keyframe(frame: &[u8]) -> bool {
    frame.first().is_some_and(|b| b & 1 == 0)
}
