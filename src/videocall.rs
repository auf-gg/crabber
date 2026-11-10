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

pub fn screen() -> Option<crate::screens::ScreenTarget> {
    SCREEN.lock().unwrap().clone()
}

fn camera() -> Option<String> {
    let all = cameras();
    let chosen = CAMERA.lock().unwrap().clone();
    chosen.filter(|c| all.iter().any(|d| &d.id == c)).or_else(|| all.into_iter().next().map(|d| d.id))
}

/// A captured picture fed to the encoder: tightly packed BGRA (stride == width * 4).
pub struct RawFrame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Size of the local preview; the remote picture is decoded at its own size, up to `REMOTE_MAX`.
pub const PREVIEW: (u32, u32) = (320, 180);
pub const REMOTE_MAX: (u32, u32) = (2560, 1440);

/// Resolution a screen share is sent at: the screen's own, or capped to a height.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Quality {
    Source,
    P1440,
    P1080,
    P720,
}

impl Quality {
    pub const ALL: [Self; 4] = [Self::Source, Self::P1440, Self::P1080, Self::P720];

    pub fn id(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::P1440 => "1440",
            Self::P1080 => "1080",
            Self::P720 => "720",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|q| q.id() == id)
    }

    /// Height it scales down to; None for the original.
    pub fn height(self) -> Option<u32> {
        match self {
            Self::Source => None,
            Self::P1440 => Some(1440),
            Self::P1080 => Some(1080),
            Self::P720 => Some(720),
        }
    }

    /// What a capture `height` px tall offers (None: not known yet): the original, which is the
    /// largest, and the presets below it.
    pub fn offered(height: Option<u32>) -> Vec<Self> {
        Self::ALL.into_iter().filter(|q| q.height().is_none_or(|h| height.is_none_or(|c| h < c))).collect()
    }

    /// The setting as it applies to a capture `height` px tall: a preset that doesn't scale it
    /// down is the original.
    pub fn effective(self, height: Option<u32>) -> Self {
        match (self.height(), height) {
            (Some(h), Some(c)) if h >= c => Self::Source,
            _ => self,
        }
    }
}

/// Bitrate for the height actually sent.
fn bitrate(height: Option<u32>) -> &'static str {
    match height {
        Some(..=720) => "1500k",
        Some(..=1080) => "2500k",
        Some(..=1440) => "4000k",
        _ => "6000k",
    }
}

/// Size of the screen being captured, while we capture it ourselves.
static CAPTURE_SIZE: Mutex<Option<(u32, u32)>> = Mutex::new(None);

/// Width and height of the shared screen or window; None when not known (not sharing, or ffmpeg
/// captures it itself).
pub fn capture_size() -> Option<(u32, u32)> {
    *CAPTURE_SIZE.lock().unwrap()
}

static QUALITY: Mutex<Quality> = Mutex::new(Quality::P1080);

/// Read whenever a screen encoder starts.
pub fn set_screen_quality(q: Quality) {
    *QUALITY.lock().unwrap() = q;
}

/// Newest decoded frame of one side (taken by the UI), its size, and when the last one arrived.
#[derive(Default)]
pub struct Slot {
    pub frame: Option<Vec<u8>>,
    pub size: (u32, u32),
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
    /// Our own capture (portal session, Windows capture) for fed frames.
    capture: Option<Box<dyn Send>>,
    _feed: Option<SyncSender<()>>,
    /// When the last keyframe was forced for the peer.
    keyed: Mutex<Option<Instant>>,
}

/// `capture_height`: the input's height when known, for the bitrate of what is really sent.
pub(crate) fn encode_args(src: Source, capture_height: Option<u32>) -> Vec<String> {
    let quality = *QUALITY.lock().unwrap();
    let sent = match (quality.height(), capture_height) {
        (Some(q), Some(c)) => Some(q.min(c)),
        (q, c) => q.or(c),
    };
    // Even sizes for yuv420p.
    let (scale, rate) = match (src, quality.height()) {
        (Source::Camera, _) => ("scale='min(1280,iw)':-2".to_string(), "1200k"),
        (Source::Screen, Some(h)) => (format!("scale=-2:'min({h},ih)'"), bitrate(sent)),
        (Source::Screen, None) => ("scale=trunc(iw/2)*2:trunc(ih/2)*2".to_string(), bitrate(sent)),
    };
    #[rustfmt::skip]
    let args = [
        "-an", "-vf", &format!("{scale},format=yuv420p"),
        "-c:v", "libvpx", "-deadline", "realtime", "-cpu-used", "8", "-lag-in-frames", "0", "-auto-alt-ref", "0",
        "-error-resilient", "1", "-g", "60", "-b:v", rate, "-maxrate", rate, "-bufsize", rate, "-threads", "4",
        "-f", "ivf", "-",
    ];
    args.iter().map(|s| s.to_string()).collect()
}

/// Frame rate of the frames we capture ourselves and feed to ffmpeg.
const FEED_FPS: u32 = 30;

/// How a source is captured: by ffmpeg itself (its input options), or by us, frames arriving on
/// the receiver while the handle lives (Wayland portal, Windows Graphics Capture).
enum Capture {
    Ffmpeg(Vec<String>),
    Fed(std::sync::mpsc::Receiver<RawFrame>, Box<dyn Send>),
}

fn capture(src: Source) -> Result<Capture, String> {
    let ffmpeg = |a: &[&str]| Ok(Capture::Ffmpeg(a.iter().map(|s| s.to_string()).collect()));
    match src {
        #[cfg(target_os = "linux")]
        Source::Camera => {
            let dev = camera().ok_or(crate::t!("call.no_camera"))?;
            ffmpeg(&["-f", "v4l2", "-i", &dev])
        }
        #[cfg(target_os = "linux")]
        Source::Screen if crate::screencast::wayland() => {
            let (tx, rx) = sync_channel::<RawFrame>(2);
            let cast = crate::screencast::start(move |f| {
                let _ = tx.try_send(RawFrame { width: f.width, height: f.height, data: f.data });
            })?;
            Ok(Capture::Fed(rx, Box::new(cast)))
        }
        #[cfg(target_os = "linux")]
        Source::Screen => ffmpeg(&["-f", "x11grab", "-framerate", "15", "-i", &std::env::var("DISPLAY").unwrap_or(":0".into())]),
        #[cfg(windows)]
        Source::Camera => {
            let name = camera().ok_or(crate::t!("call.no_camera"))?;
            ffmpeg(&["-f", "dshow", "-i", &format!("video={name}")])
        }
        // Windows Graphics Capture (GPU); GDI through ffmpeg only before Windows 10 1903.
        #[cfg(windows)]
        Source::Screen => {
            let target = SCREEN.lock().unwrap().clone().or_else(|| crate::screens::list().into_iter().next());
            let Some(target) = target else { return ffmpeg(&["-f", "gdigrab", "-framerate", "15", "-i", "desktop"]) };
            let (tx, rx) = sync_channel::<RawFrame>(2);
            match crate::wgc::start(&target, FEED_FPS, move |f| {
                let _ = tx.try_send(f);
            }) {
                Ok(cap) => Ok(Capture::Fed(rx, Box::new(cap))),
                Err(_) => Ok(Capture::Ffmpeg(crate::screens::gdigrab_input(&target))),
            }
        }
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
        match capture(src)? {
            Capture::Ffmpeg(mut input) => {
                input.extend(encode_args(src, None));
                let child = spawn_encoder(&input, sink)?;
                Ok(Self { child: Arc::new(Mutex::new(Some(child))), capture: None, _feed: None, keyed: Mutex::new(None) })
            }
            Capture::Fed(frames, capture) => Ok(Self::start_fed(frames, capture, sink)),
        }
    }

    /// Picks up a new quality: with fed frames only ffmpeg restarts, keeping the capture (and the
    /// portal session); false when the whole capture has to start again.
    pub fn restart(&self) -> bool {
        if self.capture.is_none() {
            return false;
        }
        // The feeding loop starts a new ffmpeg with the current settings on the next frame.
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        true
    }

    /// Answers the peer's keyframe request (it missed the start or lost packets): a new ffmpeg
    /// begins with a keyframe. Fed captures only, at most once a second; a still screen would
    /// otherwise wait seconds for the next regular one.
    pub fn keyframe(&self) {
        let mut keyed = self.keyed.lock().unwrap();
        if keyed.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return;
        }
        if self.restart() {
            *keyed = Some(Instant::now());
        }
    }

    /// Our frames piped into ffmpeg as raw BGRA, at most `FEED_FPS`; ffmpeg restarts when the size
    /// changes. The last frame is repeated while the screen is static so keyframes keep coming.
    fn start_fed(rx: std::sync::mpsc::Receiver<RawFrame>, capture: Box<dyn Send>, sink: Sink) -> Self {
        let child = Arc::new(Mutex::new(None::<Child>));
        let (stop, stop_rx) = sync_channel::<()>(0);
        let held = child.clone();
        std::thread::spawn(move || {
            let mut last: Option<RawFrame> = None;
            let mut size = (0, 0);
            let mut sent: Option<Instant> = None;
            let interval = Duration::from_secs(1) / FEED_FPS;
            loop {
                if !matches!(stop_rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)) {
                    break;
                }
                match rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(f) => last = Some(f),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(_) => break,
                }
                // Frames faster than the encoder's rate are dropped (the newest one is kept).
                if sent.is_some_and(|t| t.elapsed() < interval) {
                    continue;
                }
                let Some(f) = last.as_ref() else { continue };
                // Every frame: a stale encoder dropped meanwhile clears it.
                *CAPTURE_SIZE.lock().unwrap() = Some((f.width, f.height));
                let mut slot = held.lock().unwrap();
                if size != (f.width, f.height) || slot.is_none() {
                    if let Some(mut old) = slot.take() {
                        let _ = old.kill();
                        let _ = old.wait();
                    }
                    size = (f.width, f.height);
                    let fps = FEED_FPS.to_string();
                    let mut input: Vec<String> =
                        ["-f", "rawvideo", "-pix_fmt", "bgra", "-s", &format!("{}x{}", f.width, f.height), "-r", &fps, "-i", "-"].iter().map(|s| s.to_string()).collect();
                    input.extend(encode_args(Source::Screen, Some(f.height)));
                    match spawn_encoder(&input, sink.clone()) {
                        Ok(c) => *slot = Some(c),
                        Err(_) => break,
                    }
                }
                let Some(stdin) = slot.as_mut().and_then(|c| c.stdin.as_mut()) else { break };
                if stdin.write_all(&f.data).is_err() {
                    *slot = None;
                }
                sent = Some(Instant::now());
            }
        });
        Self { child, capture: Some(capture), _feed: Some(stop), keyed: Mutex::new(None) }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if self.capture.is_some() {
            *CAPTURE_SIZE.lock().unwrap() = None;
        }
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
                *s = Slot { frame: Some(buf), size: (w, h), at: Some(Instant::now()) };
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

/// Picture size from a VP8 keyframe header (after the start code 9d 01 2a), capped to `REMOTE_MAX`
/// with the aspect kept and even sides.
pub fn keyframe_size(frame: &[u8]) -> Option<(u32, u32)> {
    if !is_keyframe(frame) || frame.len() < 10 || frame[3..6] != [0x9d, 0x01, 0x2a] {
        return None;
    }
    let dim = |i: usize| (u16::from_le_bytes([frame[i], frame[i + 1]]) & 0x3fff) as f32;
    let (w, h) = (dim(6), dim(8));
    if w == 0. || h == 0. {
        return None;
    }
    let k = (REMOTE_MAX.0 as f32 / w).min(REMOTE_MAX.1 as f32 / h).min(1.);
    let even = |x: f32| ((x * k) as u32 / 2 * 2).max(2);
    Some((even(w), even(h)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_offers_follow_the_capture() {
        use Quality::{P720, P1080, P1440};
        assert_eq!(Quality::offered(Some(1080)), [Quality::Source, P720]);
        assert_eq!(Quality::offered(Some(1440)), [Quality::Source, P1080, P720]);
        assert_eq!(Quality::offered(Some(2160)), Quality::ALL);
        assert_eq!(Quality::offered(None), Quality::ALL);
        assert_eq!(P1440.effective(Some(1080)), Quality::Source);
        assert_eq!(P1080.effective(Some(1080)), Quality::Source);
        assert_eq!(P720.effective(Some(1080)), P720);
        assert_eq!(P1080.effective(None), P1080);
        // The bitrate follows what is sent, so "Full HD" of a 1080p screen is the original.
        set_screen_quality(P1080);
        let rate = |h| encode_args(Source::Screen, h).iter().skip_while(|a| *a != "-b:v").nth(1).cloned().unwrap();
        assert_eq!(rate(Some(1080)), "2500k");
        assert_eq!(rate(Some(720)), "1500k");
        set_screen_quality(Quality::Source);
        assert_eq!(rate(Some(1080)), "2500k");
        assert_eq!(rate(Some(2160)), "6000k");
        assert_eq!(rate(None), "6000k");
        set_screen_quality(P1080);
    }

    /// A still screen gets a keyframe within moments of the peer asking, not at the next regular one.
    #[test]
    fn keyframe_on_request_for_fed_frames() {
        if !crate::video::available() {
            eprintln!("ffmpeg not found, skipping");
            return;
        }
        let keys = Arc::new(Mutex::new(Vec::<Instant>::new()));
        let seen = keys.clone();
        let sink: Sink = Arc::new(Mutex::new(Box::new(move |f: Vec<u8>| {
            if is_keyframe(&f) {
                seen.lock().unwrap().push(Instant::now());
            }
        })));
        let (tx, rx) = sync_channel(2);
        tx.send(RawFrame { width: 320, height: 240, data: vec![90; 320 * 240 * 4] }).unwrap();
        let enc = Encoder::start_fed(rx, Box::new(()), sink);
        let wait = |n: usize, within: Duration| {
            let end = Instant::now() + within;
            while keys.lock().unwrap().len() < n && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(20));
            }
            keys.lock().unwrap().len()
        };
        assert_eq!(wait(1, Duration::from_secs(5)), 1, "first keyframe");
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(keys.lock().unwrap().len(), 1, "a still screen has no regular keyframe this soon");
        let asked = Instant::now();
        enc.keyframe();
        assert_eq!(wait(2, Duration::from_secs(3)), 2, "keyframe after the request");
        println!("keyframe {:?} after the request", keys.lock().unwrap()[1] - asked);
        // A second request right away is ignored (at most one a second).
        enc.keyframe();
        std::thread::sleep(Duration::from_millis(800));
        assert_eq!(keys.lock().unwrap().len(), 2);
        drop(tx);
    }

    #[test]
    fn keyframe_sizes() {
        let key = |w: u16, h: u16| [[0x10, 0x02, 0x00, 0x9d, 0x01, 0x2a].as_slice(), &w.to_le_bytes(), &h.to_le_bytes()].concat();
        assert_eq!(keyframe_size(&key(1280, 720)), Some((1280, 720)));
        // 4K comes down to the cap, aspect kept.
        assert_eq!(keyframe_size(&key(3840, 2160)), Some((2560, 1440)));
        // An odd ultrawide side stays even.
        assert_eq!(keyframe_size(&key(3441, 1440)), Some((2560, 1070)));
        let mut inter = key(1280, 720);
        inter[0] |= 1;
        assert_eq!(keyframe_size(&inter), None);
        assert_eq!(keyframe_size(&[0x10, 0x02]), None);
    }
}
