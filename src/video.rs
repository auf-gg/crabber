//! Video playback through the system ffmpeg/ffprobe (not bundled): BGRA frames for the UI, PCM to the speaker.
use crate::t;
use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};

/// Frames are resampled to a constant rate, so frame N is due at N / FPS.
const FPS: f64 = 30.0;
const MAX_W: u32 = 1280;

pub(crate) fn cmd(name: &str) -> Command {
    let mut c = Command::new(name);
    c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    c
}

/// ffmpeg and ffprobe are on PATH (checked once per run).
pub fn available() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| ["ffmpeg", "ffprobe"].iter().all(|n| cmd(n).arg("-version").status().is_ok_and(|s| s.success())))
}

pub struct Info {
    pub duration: f64,
    pub has_audio: bool,
}

pub fn probe(path: &Path) -> Result<Info, String> {
    let out = cmd("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration:stream=codec_type", "-of", "default=nw=1:nk=1"])
        .arg(path)
        .stdout(Stdio::piped())
        .output()
        .map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out.stdout);
    let duration = text.lines().find_map(|l| l.trim().parse::<f64>().ok()).ok_or(t!("video.not_video"))?;
    Ok(Info { duration, has_audio: text.lines().any(|l| l.trim() == "audio") })
}

/// First frame as PNG, at most MAX_W wide; its size is also the playback frame size.
pub fn thumbnail(src: &Path, dst: &Path) -> Result<(), String> {
    let ok = cmd("ffmpeg")
        .args(["-v", "error", "-y", "-i"])
        .arg(src)
        .args(["-frames:v", "1", "-vf", &format!("scale='min({MAX_W},iw)':-2")])
        .arg(dst)
        .status()
        .is_ok_and(|s| s.success());
    if ok && dst.is_file() { Ok(()) } else { Err(t!("video.ffmpeg_failed").into()) }
}

/// Playback from `from` seconds; dropping it stops both ffmpeg processes.
pub struct Playback {
    from: f64,
    frames: Receiver<Vec<u8>>,
    taken: u64,
    ended: bool,
    paused: Arc<AtomicBool>,
    volume: Arc<AtomicU32>,
    played: f64,
    resumed: Option<Instant>,
    video: Child,
    _audio: Option<Sender<()>>,
}

impl Playback {
    pub fn start(path: &Path, (w, h): (u32, u32), from: f64, has_audio: bool, volume: f32) -> Result<Self, String> {
        let mut video = cmd("ffmpeg")
            .args(["-v", "error", "-ss", &format!("{from:.3}"), "-i"])
            .arg(path)
            .args(["-an", "-vf", &format!("scale={w}:{h},fps={FPS}"), "-f", "rawvideo", "-pix_fmt", "bgra", "-"])
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| e.to_string())?;
        let mut out = video.stdout.take().unwrap();
        let (tx, frames) = sync_channel(4);
        std::thread::spawn(move || {
            loop {
                let mut buf = vec![0u8; (w * h * 4) as usize];
                if out.read_exact(&mut buf).is_err() || tx.send(buf).is_err() {
                    break;
                }
            }
        });
        let paused = Arc::new(AtomicBool::new(false));
        let volume = Arc::new(AtomicU32::new(volume.to_bits()));
        let audio = has_audio.then(|| start_audio(path.to_path_buf(), from, paused.clone(), volume.clone()));
        Ok(Self { from, frames, taken: 0, ended: false, paused, volume, played: 0., resumed: Some(Instant::now()), video, _audio: audio })
    }

    fn clock(&self) -> f64 {
        self.played + self.resumed.map_or(0., |t| t.elapsed().as_secs_f64())
    }

    /// Seconds from the start of the file.
    pub fn position(&self) -> f64 {
        self.from + self.clock()
    }

    pub fn set_volume(&self, v: f32) {
        self.volume.store(v.to_bits(), Ordering::Relaxed);
    }

    pub fn paused(&self) -> bool {
        self.resumed.is_none()
    }

    pub fn toggle_pause(&mut self) {
        match self.resumed.take() {
            Some(t) => self.played += t.elapsed().as_secs_f64(),
            None => self.resumed = Some(Instant::now()),
        }
        self.paused.store(self.resumed.is_none(), Ordering::Relaxed);
    }

    /// Newest frame that is due by now (BGRA), or None if the shown one is still current.
    pub fn frame(&mut self) -> Option<Vec<u8>> {
        let due = (self.clock() * FPS) as u64;
        let mut last = None;
        while self.taken <= due {
            match self.frames.try_recv() {
                Ok(f) => {
                    self.taken += 1;
                    last = Some(f);
                }
                Err(TryRecvError::Disconnected) => {
                    self.ended = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
            }
        }
        last
    }

    pub fn ended(&self) -> bool {
        self.ended
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        let _ = self.video.kill();
        let _ = self.video.wait();
    }
}

/// Streams the audio track to the default output at the device's own rate and channel count;
/// runs until the returned sender is dropped. Silent if there is no output device.
fn start_audio(path: std::path::PathBuf, from: f64, paused: Arc<AtomicBool>, volume: Arc<AtomicU32>) -> Sender<()> {
    let (stop, stop_rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let Some(output) = crate::audio::output_device() else { return };
        let Ok(cfg) = output.default_output_config() else { return };
        let (rate, ch) = (cfg.sample_rate(), cfg.channels());
        let queue = Arc::new(Mutex::new(VecDeque::<f32>::new()));
        let stream = match cfg.sample_format() {
            SampleFormat::I16 => play::<i16>(&output, cfg.config(), queue.clone(), paused, volume),
            SampleFormat::U16 => play::<u16>(&output, cfg.config(), queue.clone(), paused, volume),
            _ => play::<f32>(&output, cfg.config(), queue.clone(), paused, volume),
        };
        let Ok(stream) = stream else { return };
        if stream.play().is_err() {
            return;
        }
        let Ok(mut child) = cmd("ffmpeg")
            .args(["-v", "error", "-ss", &format!("{from:.3}"), "-i"])
            .arg(&path)
            .args(["-vn", "-f", "f32le", "-ac", &ch.to_string(), "-ar", &rate.to_string(), "-"])
            .stdout(Stdio::piped())
            .spawn()
        else {
            return;
        };
        let mut out = child.stdout.take().unwrap();
        let mut buf = [0u8; 16384];
        // Bytes of a sample split across reads, carried to the front of `buf`.
        let mut carry = 0;
        let full = (rate * ch as u32) as usize; // ~1 s ahead
        'read: loop {
            while queue.lock().unwrap().len() > full {
                if !matches!(stop_rx.try_recv(), Err(TryRecvError::Empty)) {
                    break 'read;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            match out.read(&mut buf[carry..]) {
                Ok(0) | Err(_) => {
                    let _ = stop_rx.recv();
                    break;
                }
                Ok(n) => {
                    let n = carry + n;
                    let whole = n - n % 4;
                    queue.lock().unwrap().extend(buf[..whole].chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])));
                    buf.copy_within(whole..n, 0);
                    carry = n - whole;
                }
            }
            if !matches!(stop_rx.try_recv(), Err(TryRecvError::Empty)) {
                break;
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        drop(stream);
    });
    stop
}

fn play<T>(device: &cpal::Device, config: StreamConfig, queue: Arc<Mutex<VecDeque<f32>>>, paused: Arc<AtomicBool>, volume: Arc<AtomicU32>) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                let mut q = queue.lock().unwrap();
                let on = !paused.load(Ordering::Relaxed);
                let vol = f32::from_bits(volume.load(Ordering::Relaxed));
                for s in data.iter_mut() {
                    *s = T::from_sample(if on { q.pop_front().unwrap_or(0.0) * vol } else { 0.0 });
                }
            },
            |e| eprintln!("video audio error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs ffmpeg on PATH; skipped otherwise.
    #[test]
    fn decodes_generated_clip() {
        if !available() {
            eprintln!("ffmpeg not found, skipping");
            return;
        }
        let dir = std::env::temp_dir().join(format!("crabber-video-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("clip.mp4");
        let ok = cmd("ffmpeg")
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc=size=320x240:rate=25:duration=1", "-f", "lavfi", "-i", "sine=duration=1", "-shortest"])
            .arg(&clip)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let info = probe(&clip).unwrap();
        assert!((info.duration - 1.0).abs() < 0.2, "{}", info.duration);
        assert!(info.has_audio);
        let thumb = dir.join("t.png");
        thumbnail(&clip, &thumb).unwrap();
        assert_eq!(image::image_dimensions(&thumb).unwrap(), (320, 240));
        let mut p = Playback::start(&clip, (320, 240), 0., false, 1.).unwrap();
        let mut got = 0;
        let t = Instant::now();
        while !p.ended() && t.elapsed() < Duration::from_secs(5) {
            if let Some(f) = p.frame() {
                assert_eq!(f.len(), 320 * 240 * 4);
                got += 1;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(p.ended() && got >= 20, "{got} frames");
        let _ = std::fs::remove_dir_all(dir);
    }
}
