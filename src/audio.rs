//! Call audio: microphone -> 8 kHz G.711 µ-law frames, and µ-law RTP payloads -> speaker.
//! G.711 needs no native codec library and every WebRTC stack (Conversations, Dino) offers it.
use crate::t;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample as _, SampleFormat, SizedSample, StreamConfig};

pub const RATE: u32 = 8000;

/// Chosen microphone and speaker as cpal device ids ("pipewire:…", "wasapi:…"); None = system default.
static INPUT: Mutex<Option<String>> = Mutex::new(None);
static OUTPUT: Mutex<Option<String>> = Mutex::new(None);

/// RNNoise noise suppression on the microphone; read when a capture stream opens.
static DENOISE: AtomicBool = AtomicBool::new(false);

pub fn set_denoise(on: bool) {
    DENOISE.store(on, Ordering::Relaxed);
}

pub fn set_devices(input: Option<String>, output: Option<String>) {
    *INPUT.lock().unwrap() = input;
    *OUTPUT.lock().unwrap() = output;
}

/// Host for the system default devices. On Linux that stays ALSA (routed to PipeWire/Pulse by the
/// system) even with the PipeWire host built in: its own "default" node does not open everywhere.
fn system_host() -> cpal::Host {
    #[cfg(target_os = "linux")]
    if let Ok(h) = cpal::host_from_id(cpal::HostId::Alsa) {
        return h;
    }
    cpal::default_host()
}

/// The chosen device if it is still there; an unplugged one falls back to the default.
fn chosen(id: &Mutex<Option<String>>) -> Option<cpal::Device> {
    let id: cpal::DeviceId = id.lock().unwrap().as_deref()?.parse().ok()?;
    cpal::host_from_id(id.host()).ok()?.device_by_id(&id)
}

/// Opens a stream on the chosen device, else the system default, else any listed one: on some
/// PipeWire setups the ALSA default can't capture while the PipeWire nodes work.
pub(crate) fn with_device<T>(input: bool, mut build: impl FnMut(&cpal::Device) -> Result<T, String>) -> Result<T, String> {
    let first = if input { input_device() } else { output_device() };
    let mut err = if input { t!("audio.no_mic") } else { t!("audio.no_output") }.to_string();
    let rest = || devices(input).into_iter().filter_map(|d| {
        let id: cpal::DeviceId = d.id.parse().ok()?;
        cpal::host_from_id(id.host()).ok()?.device_by_id(&id)
    });
    for dev in first.into_iter().chain(rest()) {
        match build(&dev) {
            Ok(v) => return Ok(v),
            Err(e) => err = e,
        }
    }
    Err(err)
}

pub fn input_device() -> Option<cpal::Device> {
    chosen(&INPUT).or_else(|| system_host().default_input_device())
}

pub fn output_device() -> Option<cpal::Device> {
    chosen(&OUTPUT).or_else(|| system_host().default_output_device())
}

#[derive(Clone, Debug, PartialEq)]
pub struct MediaDevice {
    pub id: String,
    pub name: String,
}

/// Devices to choose from. On Linux these are PipeWire nodes when PipeWire runs (real names,
/// headsets included); plain ALSA lists dozens of plugins, so only its per-card defaults are kept.
pub fn devices(input: bool) -> Vec<MediaDevice> {
    let list = |host: &cpal::Host, input: bool| -> Vec<MediaDevice> {
        let devs = if input { host.input_devices().map(|d| d.collect::<Vec<_>>()) } else { host.output_devices().map(|d| d.collect()) };
        devs.unwrap_or_default().into_iter().filter_map(|d| Some(MediaDevice { id: d.id().ok()?.to_string(), name: d.to_string() })).collect()
    };
    #[cfg(target_os = "linux")]
    {
        if let Ok(pw) = cpal::host_from_id(cpal::HostId::PipeWire) {
            let outputs = list(&pw, false);
            let mut out = if input { list(&pw, true) } else { outputs.clone() };
            // "…_default" aliases duplicate the system default; inputs that are also outputs are monitors.
            out.retain(|d| !d.id.ends_with("_default") && !(input && outputs.iter().any(|o| o.id == d.id)));
            return out;
        }
        let mut out = list(&system_host(), input);
        out.retain(|d| d.id.starts_with("alsa:sysdefault:"));
        out
    }
    #[cfg(not(target_os = "linux"))]
    list(&cpal::default_host(), input)
}
/// 20 ms at 8 kHz.
pub const FRAME: usize = 160;

pub fn ulaw_encode(sample: i16) -> u8 {
    const BIAS: i32 = 0x84;
    const CLIP: i32 = 32635;
    let mut s = sample as i32;
    let sign = if s < 0 {
        s = -s;
        0x80
    } else {
        0
    };
    s = s.min(CLIP) + BIAS;
    let mut exponent = 7;
    let mut mask = 0x4000;
    while exponent > 0 && s & mask == 0 {
        exponent -= 1;
        mask >>= 1;
    }
    let mantissa = (s >> (exponent + 3)) & 0x0F;
    !(sign | (exponent << 4) | mantissa) as u8
}

pub fn ulaw_decode(u: u8) -> i16 {
    const BIAS: i32 = 0x84;
    let u = !u as i32;
    let exponent = (u >> 4) & 7;
    let s = (((u & 0x0F) << 3) + BIAS) << exponent;
    (if u & 0x80 != 0 { BIAS - s } else { s - BIAS }) as i16
}

/// Box-filter decimation; good enough for voice into an 8 kHz codec.
pub(crate) struct Down {
    pub(crate) ratio: f64,
    pub(crate) pos: f64,
    pub(crate) acc: f32,
    pub(crate) n: u32,
}

impl Down {
    pub(crate) fn push(&mut self, x: f32, out: &mut Vec<f32>) {
        self.acc += x;
        self.n += 1;
        self.pos += 1.0;
        if self.pos >= self.ratio {
            out.push(self.acc / self.n as f32);
            self.acc = 0.0;
            self.n = 0;
            self.pos -= self.ratio;
        }
    }
}

/// Linear interpolation from 8 kHz up to the device rate.
pub(crate) struct Up {
    pub(crate) step: f64,
    pub(crate) frac: f64,
    pub(crate) last: f32,
}

impl Up {
    pub(crate) fn push(&mut self, x: f32, out: &mut VecDeque<f32>) {
        while self.frac < 1.0 {
            out.push_back(self.last + (x - self.last) * self.frac as f32);
            self.frac += self.step;
        }
        self.frac -= 1.0;
        self.last = x;
    }
}

/// RNNoise (nnnoiseless) works on 48 kHz audio in 10 ms frames.
const DENOISE_RATE: u32 = 48000;
const DENOISE_FRAME: usize = nnnoiseless::DenoiseState::FRAME_SIZE;

/// Microphone chain: mono samples at the device rate in, `rate` samples out; with noise
/// suppression on, they pass through RNNoise at 48 kHz first (10 ms of extra latency).
pub(crate) struct Mic {
    /// RNNoise state, the resampler to 48 kHz (none if the device runs at it) and pending samples.
    denoise: Option<(Box<nnnoiseless::DenoiseState<'static>>, Option<Up>, VecDeque<f32>)>,
    tmp: Vec<f32>,
    down: Down,
}

impl Mic {
    pub(crate) fn new(device_rate: u32, rate: u32) -> Self {
        Self::with(device_rate, rate, DENOISE.load(Ordering::Relaxed))
    }

    fn with(device_rate: u32, rate: u32, on: bool) -> Self {
        let src = if on { DENOISE_RATE } else { device_rate };
        let up = (device_rate != DENOISE_RATE).then(|| Up { step: device_rate as f64 / DENOISE_RATE as f64, frac: 0.0, last: 0.0 });
        Self {
            denoise: on.then(|| (nnnoiseless::DenoiseState::new(), up, VecDeque::new())),
            tmp: vec![],
            down: Down { ratio: src as f64 / rate as f64, pos: 0.0, acc: 0.0, n: 0 },
        }
    }

    pub(crate) fn push(&mut self, x: f32, out: &mut Vec<f32>) {
        let Some((state, up, buf)) = self.denoise.as_mut() else { return self.down.push(x, out) };
        match up {
            Some(up) => up.push(x, buf),
            None => buf.push_back(x),
        }
        while buf.len() >= DENOISE_FRAME {
            // RNNoise expects 16-bit sample magnitudes.
            let input: Vec<f32> = buf.drain(..DENOISE_FRAME).map(|s| s * 32767.0).collect();
            let mut clean = [0f32; DENOISE_FRAME];
            state.process_frame(&mut clean, &input);
            self.tmp.extend(clean.iter().map(|s| s / 32767.0));
        }
        for y in self.tmp.drain(..) {
            self.down.push(y, out);
        }
    }
}

struct Playback {
    buf: VecDeque<f32>,
    up: Up,
    rate: u32,
}

pub struct Audio {
    pub muted: Arc<AtomicBool>,
    playback: Arc<Mutex<Playback>>,
    /// Dropping it stops the streams' thread.
    stop: Mutex<std::sync::mpsc::Sender<()>>,
    on_frame: Arc<dyn Fn(Vec<u8>) + Send + Sync>,
}

impl Audio {
    /// Feeds one received µ-law RTP payload to the speaker.
    pub fn play_ulaw(&self, payload: &[u8]) {
        let mut p = self.playback.lock().unwrap();
        let Playback { buf, up, rate } = &mut *p;
        for &b in payload {
            up.push(ulaw_decode(b) as f32 / 32768.0, buf);
        }
        // Keep latency bounded if the network delivers faster than we play.
        let max = *rate as usize * 3 / 10;
        if buf.len() > max {
            let excess = buf.len() - *rate as usize / 10;
            buf.drain(..excess);
        }
    }
}

/// Opens the chosen (or default) microphone and speaker. `on_frame` receives 160-byte µ-law frames.
pub fn start(on_frame: impl Fn(Vec<u8>) + Send + Sync + 'static) -> Result<Audio, String> {
    let playback = Arc::new(Mutex::new(Playback { buf: VecDeque::new(), up: Up { step: 1.0, frac: 0.0, last: 0.0 }, rate: RATE }));
    let muted = Arc::new(AtomicBool::new(false));
    let on_frame: Arc<dyn Fn(Vec<u8>) + Send + Sync> = Arc::new(on_frame);
    let stop = open(&playback, &muted, &on_frame)?;
    Ok(Audio { muted, playback, stop: Mutex::new(stop), on_frame })
}

impl Audio {
    /// Reopens the streams on the currently chosen devices (switching mid-call).
    pub fn restart(&self) -> Result<(), String> {
        let mut stop = self.stop.lock().unwrap();
        // Close the old streams first: some devices can't be opened twice.
        *stop = std::sync::mpsc::channel().0;
        *stop = open(&self.playback, &self.muted, &self.on_frame)?;
        Ok(())
    }
}

/// Starts the streams; cpal streams are not Send, so they live on their own thread until the
/// returned sender is dropped.
fn open(playback: &Arc<Mutex<Playback>>, muted: &Arc<AtomicBool>, on_frame: &Arc<dyn Fn(Vec<u8>) + Send + Sync>) -> Result<std::sync::mpsc::Sender<()>, String> {
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let (playback, m, on_frame) = (playback.clone(), muted.clone(), on_frame.clone());
    std::thread::spawn(move || {
        let streams = (|| {
            let out_stream = with_device(false, |output| {
                let out_cfg = output.default_output_config().map_err(|e| e.to_string())?;
                let out_rate = out_cfg.sample_rate();
                *playback.lock().unwrap() = Playback { buf: VecDeque::new(), up: Up { step: RATE as f64 / out_rate as f64, frac: 0.0, last: 0.0 }, rate: out_rate };
                let stream = match out_cfg.sample_format() {
                    SampleFormat::I16 => build_output::<i16>(output, out_cfg.config(), playback.clone()),
                    SampleFormat::U16 => build_output::<u16>(output, out_cfg.config(), playback.clone()),
                    _ => build_output::<f32>(output, out_cfg.config(), playback.clone()),
                }?;
                stream.play().map_err(|e| e.to_string())?;
                Ok(stream)
            })?;
            let in_stream = with_device(true, |input| {
                let in_cfg = input.default_input_config().map_err(|e| e.to_string())?;
                let (m, on_frame) = (m.clone(), on_frame.clone());
                let send = move |f| on_frame(f);
                let stream = match in_cfg.sample_format() {
                    SampleFormat::I16 => build_input::<i16>(input, in_cfg.config(), m, send),
                    SampleFormat::U16 => build_input::<u16>(input, in_cfg.config(), m, send),
                    _ => build_input::<f32>(input, in_cfg.config(), m, send),
                }?;
                stream.play().map_err(|e| e.to_string())?;
                Ok(stream)
            })?;
            Ok::<_, String>((in_stream, out_stream))
        })();
        match streams {
            Ok(streams) => {
                let _ = init_tx.send(Ok(()));
                // Returns when the sender is dropped (hang-up or device switch).
                let _ = stop_rx.recv();
                drop(streams);
            }
            Err(e) => {
                let _ = init_tx.send(Err(e));
            }
        }
    });
    init_rx.recv().map_err(|_| t!("audio.stream_ended").to_string())??;
    Ok(stop_tx)
}

/// Ringing played on the default speaker until dropped.
pub struct Tone(#[allow(dead_code)] std::sync::mpsc::Sender<()>);

/// `incoming`: the ring for a call to us; otherwise the ringback heard while calling out.
pub fn tone(incoming: bool) -> Tone {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let res = (|| {
            let output = output_device().ok_or(t!("audio.no_output"))?;
            let cfg = output.default_output_config().map_err(|e| e.to_string())?;
            let stream = match cfg.sample_format() {
                SampleFormat::I16 => build_tone::<i16>(&output, cfg.config(), incoming),
                SampleFormat::U16 => build_tone::<u16>(&output, cfg.config(), incoming),
                _ => build_tone::<f32>(&output, cfg.config(), incoming),
            }?;
            stream.play().map_err(|e| e.to_string())?;
            // Returns once `Tone` (the sender) is dropped; the stream stops with it.
            let _ = rx.recv();
            Ok::<_, String>(())
        })();
        if let Err(e) = res {
            eprintln!("ring tone: {e}");
        }
    });
    Tone(tx)
}

/// Kalimba motifs: the incoming ring every 3 s, the ringback (two soft notes) every 2.5 s.
fn ring_sample(incoming: bool, t: f64) -> f32 {
    use std::f64::consts::TAU;
    // (start s, MIDI note)
    const RING: &[(f64, f64)] = &[(0.0, 79.), (0.14, 84.), (0.28, 86.), (0.42, 91.), (0.7, 86.), (0.84, 91.)]; // G5 C6 D6 G6, D6 G6
    const RINGBACK: &[(f64, f64)] = &[(0.0, 86.), (0.25, 79.)]; // D6, G5
    let (period, notes, gain) = if incoming { (3.0, RING, 0.169) } else { (2.5, RINGBACK, 0.17) };
    let p = t % period;
    let s: f64 = notes
        .iter()
        .filter(|(a, _)| p >= *a)
        .map(|&(a, n)| {
            let (dt, f) = (p - a, 440. * 2f64.powf((n - 69.) / 12.));
            let env = |attack: f64, decay: f64| (dt / attack).min(1.) * (-dt / decay).exp();
            // Metal tine: fundamental + an inharmonic overtone that dies fast.
            env(0.002, 0.45) * (TAU * f * dt).sin() + 0.5 * env(0.001, 0.05) * (TAU * f * 5.4 * dt).sin()
        })
        .sum();
    (gain * s) as f32
}

fn build_tone<T>(device: &cpal::Device, config: StreamConfig, incoming: bool) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let rate = config.sample_rate as f64;
    let mut n = 0u64;
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                for frame in data.chunks_mut(channels) {
                    let v = T::from_sample(ring_sample(incoming, n as f64 / rate));
                    n += 1;
                    frame.iter_mut().for_each(|s| *s = v);
                }
            },
            |e| eprintln!("ring tone error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())
}

fn build_input<T>(device: &cpal::Device, config: StreamConfig, muted: Arc<AtomicBool>, on_frame: impl Fn(Vec<u8>) + Send + 'static) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    let mut mic = Mic::new(config.sample_rate, RATE);
    let mut pcm: Vec<f32> = Vec::with_capacity(FRAME * 2);
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                for frame in data.chunks(channels) {
                    let mono = frame.iter().map(|&s| f32::from_sample(s)).sum::<f32>() / channels as f32;
                    mic.push(mono, &mut pcm);
                }
                while pcm.len() >= FRAME {
                    let silent = muted.load(Ordering::Relaxed);
                    let frame: Vec<u8> = pcm
                        .drain(..FRAME)
                        .map(|x| ulaw_encode(if silent { 0 } else { (x.clamp(-1.0, 1.0) * 32767.0) as i16 }))
                        .collect();
                    on_frame(frame);
                }
            },
            |e| eprintln!("audio input error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())
}

fn build_output<T>(device: &cpal::Device, config: StreamConfig, playback: Arc<Mutex<Playback>>) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                let mut p = playback.lock().unwrap();
                for frame in data.chunks_mut(channels) {
                    let v = T::from_sample(p.buf.pop_front().unwrap_or(0.0));
                    frame.iter_mut().for_each(|s| *s = v);
                }
            },
            |e| eprintln!("audio output error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ulaw_roundtrip_is_close() {
        for s in [-32768i16, -10000, -100, -1, 0, 1, 100, 1000, 10000, 32767] {
            let d = ulaw_decode(ulaw_encode(s)) as i32;
            // µ-law step size grows with amplitude: ~3% relative error, a few LSB near zero.
            assert!((d - s as i32).abs() <= (s as i32).abs() / 16 + 8, "{s} -> {d}");
        }
        assert_eq!(ulaw_encode(0), 0xFF, "digital silence");
    }

    #[test]
    fn ring_patterns() {
        let peak = |incoming, from: f64, to: f64| (0..((to - from) * 8000.) as usize).map(|i| ring_sample(incoming, from + i as f64 / 8000.).abs()).fold(0f32, f32::max);
        for (incoming, period) in [(true, 3.0), (false, 2.5)] {
            assert!(peak(incoming, 0.0, 0.2) > 0.1, "{incoming}: motif plays");
            assert!(peak(incoming, 0.0, period) < 0.6, "{incoming}: no clipping");
            assert!(peak(incoming, period - 0.3, period) < 0.02, "{incoming}: fades out before repeating");
            assert!(peak(incoming, period, period + 0.2) > 0.1, "{incoming}: repeats");
        }
    }

    fn noise(n: usize) -> Vec<f32> {
        let mut seed = 1u32;
        (0..n)
            .map(|_| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                (seed >> 8) as f32 / (1 << 24) as f32 * 0.04 - 0.02
            })
            .collect()
    }

    fn run_mic(on: bool, device_rate: u32, rate: u32, input: &[f32]) -> Vec<f32> {
        let mut mic = Mic::with(device_rate, rate, on);
        let mut out = vec![];
        input.iter().for_each(|&x| mic.push(x, &mut out));
        out
    }

    /// At 48 kHz the chain hands RNNoise exactly its 10 ms frames, in its 16-bit scale.
    #[test]
    fn denoise_matches_rnnoise() {
        let input = noise(48000);
        let got = run_mic(true, 48000, 48000, &input);
        let mut st = nnnoiseless::DenoiseState::new();
        let mut want = vec![];
        for f in input.as_chunks::<DENOISE_FRAME>().0 {
            let scaled: Vec<f32> = f.iter().map(|x| x * 32767.0).collect();
            let mut out = [0f32; DENOISE_FRAME];
            st.process_frame(&mut out, &scaled);
            want.extend(out.iter().map(|x| x / 32767.0));
        }
        assert_eq!(got.len(), want.len());
        assert!(got.iter().zip(&want).all(|(a, b)| (a - b).abs() < 1e-6));
        assert!(got.iter().map(|x| x * x).sum::<f32>() < input.iter().map(|x| x * x).sum::<f32>());
    }

    /// From 44.1 kHz through the 48 kHz denoiser down to the 8 kHz call rate: the rate holds.
    #[test]
    fn denoise_keeps_rate() {
        let input = noise(44100 * 2);
        assert!((run_mic(false, 44100, RATE, &input).len() as i64 - 16000).abs() < 10);
        // Up to one 10 ms frame stays buffered in the denoiser.
        let n = run_mic(true, 44100, RATE, &input).len() as i64;
        assert!((n - 16000).abs() <= 90, "{n}");
    }

    #[test]
    fn resamplers_keep_rate() {
        let mut down = Down { ratio: 6.0, pos: 0.0, acc: 0.0, n: 0 };
        let mut out = vec![];
        (0..48000).for_each(|_| down.push(0.5, &mut out));
        assert_eq!(out.len(), 8000);
        assert!(out.iter().all(|&x| (x - 0.5).abs() < 1e-6));

        let mut up = Up { step: 8000.0 / 44100.0, frac: 0.0, last: 0.0 };
        let mut buf = VecDeque::new();
        (0..8000).for_each(|_| up.push(0.25, &mut buf));
        assert!((buf.len() as i32 - 44100).abs() <= 6, "{}", buf.len());
    }
}
