//! Call audio: microphone -> 8 kHz G.711 µ-law frames, and µ-law RTP payloads -> speaker.
//! G.711 needs no native codec library and every WebRTC stack (Conversations, Dino) offers it.
use crate::t;
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample as _, SampleFormat, SizedSample, StreamConfig};

/// G.711 µ-law runs at 8 kHz.
pub const RATE: u32 = 8000;
/// Calls run at 48 kHz in 20 ms frames: Opus' own rate; µ-law is resampled from it.
pub const CALL_RATE: u32 = 48000;
pub const CALL_FRAME: usize = 960;

/// Chosen microphone and speaker as cpal device ids ("pipewire:…", "wasapi:…"); None = system default.
static INPUT: Mutex<Option<String>> = Mutex::new(None);
static OUTPUT: Mutex<Option<String>> = Mutex::new(None);

/// RNNoise noise suppression on the microphone; read when a capture stream opens.
static DENOISE: AtomicBool = AtomicBool::new(false);

pub fn set_denoise(on: bool) {
    DENOISE.store(on, Ordering::Relaxed);
}

/// Call volumes (1.0 = unchanged), applied live: what we send and what we hear.
static MIC_GAIN: AtomicU32 = AtomicU32::new(0x3F80_0000);
static OUT_GAIN: AtomicU32 = AtomicU32::new(0x3F80_0000);

pub fn set_volumes(mic: f32, out: f32) {
    MIC_GAIN.store(mic.to_bits(), Ordering::Relaxed);
    OUT_GAIN.store(out.to_bits(), Ordering::Relaxed);
}

/// Loudness (RMS) of call audio: ours as sent, the peer's as received. The UI takes the loudest
/// frame since its last look, so silence reads as 0 once frames stop.
static LEVELS: [AtomicU32; 2] = [AtomicU32::new(0), AtomicU32::new(0)];

pub fn note_level(peer: bool, samples: &[f32]) {
    let rms = (samples.iter().map(|x| x * x).sum::<f32>() / samples.len().max(1) as f32).sqrt();
    // Non-negative floats order like their bits.
    LEVELS[peer as usize].fetch_max(rms.to_bits(), Ordering::Relaxed);
}

pub fn take_level(peer: bool) -> f32 {
    f32::from_bits(LEVELS[peer as usize].swap(0, Ordering::Relaxed))
}

/// The same for each participant of a group call, by the key its audio comes in under.
static SOURCE_LEVELS: Mutex<BTreeMap<u64, f32>> = Mutex::new(BTreeMap::new());

pub fn take_source_levels() -> BTreeMap<u64, f32> {
    std::mem::take(&mut *SOURCE_LEVELS.lock().unwrap())
}

fn gain(g: &AtomicU32) -> f32 {
    f32::from_bits(g.load(Ordering::Relaxed))
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
        let pipewire = host.id() != system_host().id();
        devs.unwrap_or_default()
            .into_iter()
            // PipeWire also lists other apps' streams; only nodes made by a driver (factory) are devices.
            .filter(|d| !pipewire || d.description().is_ok_and(|x| x.driver().is_some()))
            .filter_map(|d| Some(MediaDevice { id: d.id().ok()?.to_string(), name: d.to_string() }))
            .collect()
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

/// A 20 ms call frame (48 kHz) as a µ-law payload: every 6 samples averaged down to 8 kHz.
pub fn ulaw_frame(pcm: &[f32]) -> Vec<u8> {
    pcm.chunks(6).map(|c| ulaw_encode(((c.iter().sum::<f32>() / c.len() as f32).clamp(-1.0, 1.0) * 32767.0) as i16)).collect()
}

/// A µ-law payload as 8 kHz samples.
pub fn ulaw_samples(payload: &[u8]) -> Vec<f32> {
    payload.iter().map(|&b| ulaw_decode(b) as f32 / 32768.0).collect()
}

/// Opus for calls: 48 kHz mono, 20 ms frames, through the pure-Rust libopus port.
pub struct OpusEncoder(*mut unsafe_libopus::OpusEncoder);
pub struct OpusDecoder(*mut unsafe_libopus::OpusDecoder);

// SAFETY: the codec state is plain memory owned by the value, with no thread affinity.
unsafe impl Send for OpusEncoder {}
unsafe impl Send for OpusDecoder {}

impl OpusEncoder {
    pub fn new() -> Result<Self, String> {
        let mut err = 0;
        let enc = unsafe { unsafe_libopus::opus_encoder_create(CALL_RATE as i32, 1, unsafe_libopus::OPUS_APPLICATION_VOIP, &mut err) };
        if enc.is_null() || err != unsafe_libopus::OPUS_OK {
            return Err(t!("audio.opus_error", code = err));
        }
        Ok(Self(enc))
    }

    /// One packet for one 20 ms frame.
    pub fn encode(&mut self, pcm: &[f32]) -> Option<Vec<u8>> {
        let mut packet = [0u8; 1500];
        let n = unsafe { unsafe_libopus::opus_encode_float(self.0, pcm.as_ptr(), pcm.len() as i32, packet.as_mut_ptr(), packet.len() as i32) };
        (n > 0).then(|| packet[..n as usize].to_vec())
    }
}

impl OpusDecoder {
    pub fn new() -> Result<Self, String> {
        let mut err = 0;
        let dec = unsafe { unsafe_libopus::opus_decoder_create(CALL_RATE as i32, 1, &mut err) };
        if dec.is_null() || err != unsafe_libopus::OPUS_OK {
            return Err(t!("audio.opus_error", code = err));
        }
        Ok(Self(dec))
    }

    /// 48 kHz mono samples (a stereo stream is mixed down by the decoder).
    pub fn decode(&mut self, packet: &[u8]) -> Vec<f32> {
        let mut pcm = vec![0f32; 5760]; // 120 ms, the longest Opus packet
        let n = unsafe { unsafe_libopus::opus_decode_float(self.0, packet.as_ptr(), packet.len() as i32, pcm.as_mut_ptr(), 5760, 0) };
        pcm.truncate(n.max(0) as usize);
        pcm
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        unsafe { unsafe_libopus::opus_encoder_destroy(self.0) };
    }
}

impl Drop for OpusDecoder {
    fn drop(&mut self) {
        unsafe { unsafe_libopus::opus_decoder_destroy(self.0) };
    }
}

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

/// One audio RTP packet from the peer.
pub struct RtpAudio {
    pub seq: u16,
    pub ts: u32,
    pub ssrc: u32,
    /// G.711 µ-law; Opus otherwise.
    pub pcmu: bool,
    pub payload: Vec<u8>,
}

/// Buffering before the peer is played, ms: at least this, more as the network is uneven.
const MIN_DELAY_MS: f64 = 40.0;
const START_DELAY_MS: f64 = 60.0;
const MAX_DELAY_MS: f64 = 300.0;
/// Missing frames in a row bridged by concealment; after that playout waits for packets again.
const MAX_CONCEALED: u32 = 5;

/// The peer's packets wait here in sequence order and the speaker plays them by its own clock.
/// The network delivers unevenly (a phone on Wi-Fi or LTE all the more); played as they came, a
/// late packet left the speaker empty, and the sound dropping to zero and back was heard as a thump.
struct Jitter {
    /// By unwrapped sequence number: µ-law or not, and the payload.
    packets: BTreeMap<u64, (bool, Vec<u8>)>,
    ssrc: Option<u32>,
    /// Highest sequence number seen, unwrapped.
    seq: Option<u64>,
    /// Next to play; None while (re)buffering.
    next: Option<u64>,
    /// First arrival, last RTP timestamp and the unwrapped one, for transit times.
    origin: Option<(Instant, u32, i64)>,
    /// Arrival and transit time (ms) of recent packets: how uneven the network is.
    transits: VecDeque<(Instant, f64)>,
    /// What is buffered before playing starts, ms.
    target: f64,
    opus: Option<OpusDecoder>,
    /// Length (samples) and rate of the last frame.
    frame: (usize, u32),
    /// Frames concealed in a row.
    concealed: u32,
    /// Last sample played, and whether silence came since (the next sound then fades in).
    tail: f32,
    silent: bool,
}

impl Jitter {
    fn new() -> Self {
        Self {
            packets: BTreeMap::new(),
            ssrc: None,
            seq: None,
            next: None,
            origin: None,
            transits: VecDeque::new(),
            target: START_DELAY_MS,
            opus: None,
            frame: (CALL_FRAME, CALL_RATE),
            concealed: 0,
            tail: 0.0,
            silent: true,
        }
    }

    fn receive(&mut self, p: RtpAudio, now: Instant) {
        if self.ssrc != Some(p.ssrc) {
            // Another stream: start over.
            *self = Self { ssrc: Some(p.ssrc), tail: self.tail, silent: self.silent, ..Self::new() };
        }
        let seq = match self.seq {
            None => (1 << 32) | p.seq as u64,
            Some(last) => unwrap_seq(last, p.seq),
        };
        self.seq = Some(self.seq.map_or(seq, |s| s.max(seq)));
        // Transit time up to an unknown constant: arrival minus the sender's clock.
        let clock = if p.pcmu { RATE } else { CALL_RATE } as f64;
        let (start, raw, ts) = self.origin.get_or_insert((now, p.ts, 0));
        *ts += p.ts.wrapping_sub(*raw) as i32 as i64;
        *raw = p.ts;
        let transit = now.duration_since(*start).as_secs_f64() * 1000.0 - *ts as f64 * 1000.0 / clock;
        self.transits.push_back((now, transit));
        while self.transits.front().is_some_and(|(t, _)| now.duration_since(*t) > Duration::from_secs(10)) {
            self.transits.pop_front();
        }
        // Enough buffering for 95% of packets to be in time.
        if self.transits.len() >= 50 {
            let mut d: Vec<f64> = self.transits.iter().map(|t| t.1).collect();
            d.sort_by(f64::total_cmp);
            self.target = (d[d.len() * 95 / 100] - d[0] + self.frame_ms()).clamp(MIN_DELAY_MS, MAX_DELAY_MS);
        }
        // Its turn has passed.
        if self.next.is_some_and(|n| seq < n) {
            return;
        }
        self.packets.insert(seq, (p.pcmu, p.payload));
        while self.packets.len() > 500 {
            self.packets.pop_first();
        }
    }

    /// Our sound is off: nothing is kept; playout buffers again once it is back on.
    fn stop(&mut self) {
        self.packets.clear();
        self.next = None;
    }

    fn frame_ms(&self) -> f64 {
        self.frame.0 as f64 * 1000.0 / self.frame.1 as f64
    }

    fn buffered_ms(&self) -> f64 {
        let (Some((&first, _)), Some((&last, _))) = (self.packets.first_key_value(), self.packets.last_key_value()) else { return 0.0 };
        (last + 1 - self.next.unwrap_or(first).min(first)) as f64 * self.frame_ms()
    }

    /// The next frame to play and its rate.
    fn pull(&mut self) -> (Vec<f32>, u32) {
        loop {
            let Some(next) = self.next else {
                if self.buffered_ms() < self.target {
                    return self.fade_out();
                }
                self.next = self.packets.first_key_value().map(|(&k, _)| k);
                self.concealed = 0;
                continue;
            };
            if let Some((pcmu, payload)) = self.packets.remove(&next) {
                self.next = Some(next + 1);
                let Some((pcm, rate)) = self.decode(pcmu, &payload) else { return self.conceal() };
                // Behind (a burst after a stall, or a sender whose clock runs fast): quiet frames
                // are skipped, and any once far behind.
                let excess = self.buffered_ms() - self.target;
                if excess > 200.0 || (excess > 40.0 && rms(&pcm) < 0.003) {
                    continue;
                }
                self.concealed = 0;
                return self.play(pcm, rate);
            }
            match self.packets.first_key_value().map(|(&k, _)| k) {
                // The stream jumped ahead: go on from what is there.
                Some(first) if first > next + 50 => self.next = Some(first),
                // Lost: concealed, and on to the next.
                Some(_) => {
                    self.next = Some(next + 1);
                    return self.conceal();
                }
                // Nothing came: late. Concealed without moving on, so it still plays when it
                // arrives (the buffer grows by a frame); after a while, buffer again.
                None if self.concealed < MAX_CONCEALED => return self.conceal(),
                None => {
                    self.next = None;
                    return self.fade_out();
                }
            }
        }
    }

    fn decode(&mut self, pcmu: bool, payload: &[u8]) -> Option<(Vec<f32>, u32)> {
        let (pcm, rate) = if pcmu {
            (ulaw_samples(payload), RATE)
        } else {
            if self.opus.is_none() {
                self.opus = OpusDecoder::new().ok();
            }
            (self.opus.as_mut()?.decode(payload), CALL_RATE)
        };
        if pcm.is_empty() {
            return None;
        }
        self.frame = (pcm.len(), rate);
        Some((pcm, rate))
    }

    /// A missing frame: the sound fades out and comes back in. Not Opus' own concealment (PLC,
    /// FEC): in unsafe-libopus it turns quiet audio (silence, a muted peer, the pauses in speech)
    /// into a full-scale burst and spoils the next frame too, heard as a double thump whenever
    /// packets ran late (a screen share filling the link).
    fn conceal(&mut self) -> (Vec<f32>, u32) {
        self.concealed += 1;
        self.fade_out()
    }

    /// Silence, reached gradually from the last sample played.
    fn fade_out(&mut self) -> (Vec<f32>, u32) {
        let (n, rate) = self.frame;
        let tail = std::mem::take(&mut self.tail);
        self.silent = true;
        ((0..n).map(|i| tail * (1.0 - (i + 1) as f32 / n as f32)).collect(), rate)
    }

    fn play(&mut self, mut pcm: Vec<f32>, rate: u32) -> (Vec<f32>, u32) {
        if std::mem::take(&mut self.silent) {
            // 2.5 ms in, so sound after silence doesn't start with a step.
            let n = (rate as usize / 400).min(pcm.len());
            for (i, x) in pcm[..n].iter_mut().enumerate() {
                *x *= i as f32 / n as f32;
            }
        }
        self.tail = pcm.last().copied().unwrap_or(0.0);
        note_level(true, &pcm);
        (pcm, rate)
    }
}

/// `seq` unwrapped to the 64-bit number nearest to `last`.
fn unwrap_seq(last: u64, seq: u16) -> u64 {
    let c = (last & !0xFFFF) | seq as u64;
    [c - 0x10000, c, c + 0x10000].into_iter().min_by_key(|x| x.abs_diff(last)).unwrap_or(c)
}

fn rms(pcm: &[f32]) -> f32 {
    (pcm.iter().map(|x| x * x).sum::<f32>() / pcm.len().max(1) as f32).sqrt()
}

/// One participant of a group call: its own jitter buffer, and samples decoded but not played yet
/// (its frames need not be 20 ms).
struct Source {
    jitter: Jitter,
    pending: VecDeque<f32>,
}

struct Playback {
    buf: VecDeque<f32>,
    up: Up,
    /// Device rate, and the rate of what is fed in (8 kHz µ-law or 48 kHz Opus).
    rate: u32,
    src: u32,
    /// The peer of a 1:1 call.
    jitter: Jitter,
    /// Group call participants by key, mixed together.
    sources: BTreeMap<u64, Source>,
}

impl Playback {
    fn new() -> Self {
        Self { buf: VecDeque::new(), up: Up { step: 1.0, frac: 0.0, last: 0.0 }, rate: CALL_RATE, src: CALL_RATE, jitter: Jitter::new(), sources: BTreeMap::new() }
    }

    /// 20 ms of every participant summed, at the call rate.
    fn mix(&mut self) -> Vec<f32> {
        let mut mixed = vec![0f32; CALL_FRAME];
        let mut levels = SOURCE_LEVELS.lock().unwrap();
        for (key, s) in self.sources.iter_mut() {
            while s.pending.len() < CALL_FRAME {
                let (pcm, rate) = s.jitter.pull();
                if pcm.is_empty() {
                    s.pending.resize(CALL_FRAME, 0.0);
                    break;
                }
                // Group calls are Opus; µ-law would come at 8 kHz.
                let repeat = (CALL_RATE / rate).max(1) as usize;
                s.pending.extend(pcm.into_iter().flat_map(|x| std::iter::repeat_n(x, repeat)));
            }
            let level = levels.entry(*key).or_default();
            let mut sum = 0.0;
            for (m, x) in mixed.iter_mut().zip(s.pending.drain(..CALL_FRAME)) {
                *m += x;
                sum += x * x;
            }
            *level = level.max((sum / CALL_FRAME as f32).sqrt());
        }
        mixed
    }

    /// Takes the next frame from the jitter buffer(s) into `buf`, at the device rate.
    fn refill(&mut self) {
        let (pcm, rate) = if self.sources.is_empty() { self.jitter.pull() } else { (self.mix(), CALL_RATE) };
        if rate != self.src {
            self.src = rate;
            self.up.step = rate as f64 / self.rate as f64;
        }
        let g = gain(&OUT_GAIN);
        for x in pcm {
            self.up.push((x * g).clamp(-1.0, 1.0), &mut self.buf);
        }
    }
}

pub struct Audio {
    pub muted: Arc<AtomicBool>,
    /// The peer is not played (our sound is off).
    pub deafened: AtomicBool,
    playback: Arc<Mutex<Playback>>,
    /// Dropping it stops the streams' thread.
    stop: Mutex<std::sync::mpsc::Sender<()>>,
    on_frame: Arc<dyn Fn(Vec<f32>) + Send + Sync>,
}

impl Audio {
    /// Queues a packet from the peer; the speaker plays it in its turn.
    pub fn receive(&self, packet: RtpAudio) {
        let mut p = self.playback.lock().unwrap();
        if self.deafened.load(Ordering::Relaxed) {
            return p.jitter.stop();
        }
        p.jitter.receive(packet, Instant::now());
    }

    /// Queues a packet from group call participant `key`; all participants are played mixed.
    pub fn receive_from(&self, key: u64, packet: RtpAudio) {
        let mut p = self.playback.lock().unwrap();
        let s = p.sources.entry(key).or_insert_with(|| Source { jitter: Jitter::new(), pending: VecDeque::new() });
        if self.deafened.load(Ordering::Relaxed) {
            return s.jitter.stop();
        }
        s.jitter.receive(packet, Instant::now());
    }

    /// The participant left: nothing more is played for it.
    pub fn remove_source(&self, key: u64) {
        self.playback.lock().unwrap().sources.remove(&key);
        SOURCE_LEVELS.lock().unwrap().remove(&key);
    }
}

/// Opens the chosen (or default) microphone and speaker. `on_frame` receives 20 ms frames of
/// 48 kHz mono samples, already muted or scaled by the microphone volume.
pub fn start(on_frame: impl Fn(Vec<f32>) + Send + Sync + 'static) -> Result<Audio, String> {
    let playback = Arc::new(Mutex::new(Playback::new()));
    let muted = Arc::new(AtomicBool::new(false));
    let on_frame: Arc<dyn Fn(Vec<f32>) + Send + Sync> = Arc::new(on_frame);
    let stop = open(&playback, &muted, &on_frame)?;
    Ok(Audio { muted, deafened: AtomicBool::new(false), playback, stop: Mutex::new(stop), on_frame })
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
fn open(playback: &Arc<Mutex<Playback>>, muted: &Arc<AtomicBool>, on_frame: &Arc<dyn Fn(Vec<f32>) + Send + Sync>) -> Result<std::sync::mpsc::Sender<()>, String> {
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let (playback, m, on_frame) = (playback.clone(), muted.clone(), on_frame.clone());
    std::thread::spawn(move || {
        let streams = (|| {
            let out_stream = with_device(false, |output| {
                let out_cfg = output.default_output_config().map_err(|e| e.to_string())?;
                let out_rate = out_cfg.sample_rate();
                {
                    // Another device: what is buffered for the peer stays.
                    let mut p = playback.lock().unwrap();
                    p.buf.clear();
                    (p.rate, p.src) = (out_rate, CALL_RATE);
                    p.up = Up { step: CALL_RATE as f64 / out_rate as f64, frac: 0.0, last: 0.0 };
                }
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

fn build_input<T>(device: &cpal::Device, config: StreamConfig, muted: Arc<AtomicBool>, on_frame: impl Fn(Vec<f32>) + Send + 'static) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    let mut mic = Mic::new(config.sample_rate, CALL_RATE);
    let mut pcm: Vec<f32> = Vec::with_capacity(CALL_FRAME * 2);
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                for frame in data.chunks(channels) {
                    let mono = frame.iter().map(|&s| f32::from_sample(s)).sum::<f32>() / channels as f32;
                    mic.push(mono, &mut pcm);
                }
                while pcm.len() >= CALL_FRAME {
                    let g = if muted.load(Ordering::Relaxed) { 0.0 } else { gain(&MIC_GAIN) };
                    let frame: Vec<f32> = pcm.drain(..CALL_FRAME).map(|x| (x * g).clamp(-1.0, 1.0)).collect();
                    note_level(false, &frame);
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
                    if p.buf.is_empty() {
                        p.refill();
                    }
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

    fn pcmu(seq: u16, v: f32) -> RtpAudio {
        RtpAudio { seq, ts: seq as u32 * FRAME as u32, ssrc: 1, pcmu: true, payload: vec![ulaw_encode((v * 32767.0) as i16); FRAME] }
    }

    fn mid(f: (Vec<f32>, u32)) -> f32 {
        f.0[f.0.len() / 2]
    }

    #[test]
    fn sequence_numbers_unwrap() {
        let base = 1u64 << 32;
        assert_eq!(unwrap_seq(base | 65535, 0), base + 65536);
        assert_eq!(unwrap_seq(base + 65536, 65535), base | 65535);
        assert_eq!(unwrap_seq(base | 100, 98), base | 98);
    }

    #[test]
    fn jitter_buffer_orders_conceals_and_waits_for_late_packets() {
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let near = |x: f32, v: f32| (x - v).abs() < 0.02;
        let mut j = Jitter::new();
        // 60 ms are buffered before playing.
        j.receive(pcmu(10, 0.1), at(0));
        assert!(j.pull().0.iter().all(|&x| x == 0.0), "still buffering");
        j.receive(pcmu(12, 0.3), at(20));
        j.receive(pcmu(11, 0.2), at(25));
        // In order, whatever order they came in.
        for v in [0.1, 0.2, 0.3] {
            assert!(near(mid(j.pull()), v));
        }
        // 13 is lost: faded out from where the sound was, never a step to zero; then 14.
        j.receive(pcmu(14, 0.5), at(60));
        let (gap, _) = j.pull();
        assert!(near(gap[0], 0.3) && gap.windows(2).all(|w| (w[0] - w[1]).abs() < 0.01) && gap[FRAME - 1] == 0.0);
        let (back, _) = j.pull();
        assert!(back[0].abs() < 0.03 && near(back[FRAME / 2], 0.5), "fades back in");
        // Nothing came: 15 is late, not lost; it plays once it arrives.
        assert!(j.pull().0[0] > 0.4);
        j.receive(pcmu(15, 0.6), at(110));
        assert!(near(mid(j.pull()), 0.6));
        // One whose turn has passed is dropped.
        j.receive(pcmu(13, 0.9), at(120));
        assert!(j.pull().0.iter().all(|&x| x < 0.61));
        // A long silence: playout buffers again and starts over with what comes next.
        for _ in 0..MAX_CONCEALED + 2 {
            j.pull();
        }
        assert_eq!(j.next, None);
        for (i, seq) in (40..43).enumerate() {
            j.receive(pcmu(seq, 0.4), at(1000 + 20 * i as u64));
        }
        assert!(near(mid(j.pull()), 0.4));
    }

    /// Group call: every participant has its own buffer; what is played is their sum, and each
    /// one's loudness is kept apart for the "speaking" marks.
    #[test]
    fn group_sources_are_mixed() {
        let t0 = Instant::now();
        let mut p = Playback::new();
        for (key, v) in [(1u64, 0.1f32), (2, 0.2)] {
            let s = p.sources.entry(key).or_insert_with(|| Source { jitter: Jitter::new(), pending: VecDeque::new() });
            for seq in 0..4 {
                s.jitter.receive(RtpAudio { ssrc: key as u32, ..pcmu(seq, v) }, t0);
            }
        }
        let frame = p.mix();
        assert_eq!(frame.len(), CALL_FRAME);
        assert!((frame[CALL_FRAME / 2] - 0.3).abs() < 0.02, "{}", frame[CALL_FRAME / 2]);
        let levels = take_source_levels();
        assert!(levels[&2] > levels[&1] && levels[&1] > 0.05, "{levels:?}");
        // A participant that sent nothing yet is silence, not a stall.
        p.sources.insert(3, Source { jitter: Jitter::new(), pending: VecDeque::new() });
        assert!((p.mix()[CALL_FRAME / 2] - 0.3).abs() < 0.02);
    }

    /// Quiet Opus audio (a muted peer, a pause) with packets lost and late: nothing may come out
    /// louder than what was sent. Opus' concealment in unsafe-libopus made full-scale bursts here.
    #[test]
    fn missing_packets_in_quiet_audio_stay_quiet() {
        let mut enc = OpusEncoder::new().unwrap();
        let mut rng = 7u32;
        let t0 = Instant::now();
        let mut j = Jitter::new();
        let mut loudest = 0f32;
        for i in 0..200u32 {
            let pcm: Vec<f32> = (0..CALL_FRAME).map(|_| {
                rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
                ((rng >> 8) as f32 / 8388608.0 - 1.0) * 1e-3
            }).collect();
            let payload = enc.encode(&pcm).unwrap();
            // Every 10th packet is lost, every 7th comes 100 ms late.
            if i % 10 != 5 {
                let late = if i % 7 == 3 { 100 } else { 0 };
                j.receive(RtpAudio { seq: i as u16, ts: i * 960, ssrc: 9, pcmu: false, payload }, t0 + Duration::from_millis(i as u64 * 20 + late));
            }
            let (out, _) = j.pull();
            loudest = out.iter().fold(loudest, |m, x| m.max(x.abs()));
        }
        assert!(loudest < 0.01, "burst of {loudest}");
    }

    /// A phone on Wi-Fi: packets of a tone come 0..80 ms late, sequence numbers wrap; the speaker
    /// takes a frame every 20 ms. The sound must never jump (a thump), and once the buffer has
    /// learned the network nothing may be missing.
    #[test]
    fn uneven_network_plays_smoothly() {
        let mut enc = OpusEncoder::new().unwrap();
        let mut rng = 12345u32;
        let mut arrivals: Vec<(u64, RtpAudio)> = (0..500u32)
            .map(|i| {
                let pcm: Vec<f32> = (0..CALL_FRAME).map(|k| ((i as usize * CALL_FRAME + k) as f32 * std::f32::consts::TAU * 220. / 48000.).sin() * 0.5).collect();
                rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
                let late = (rng >> 16) as u64 % 80;
                (i as u64 * 20 + late, RtpAudio { seq: 65400u16.wrapping_add(i as u16), ts: i * 960, ssrc: 7, pcmu: false, payload: enc.encode(&pcm).unwrap() })
            })
            .collect();
        arrivals.sort_by_key(|a| a.0);
        let t0 = Instant::now();
        let mut arrivals = arrivals.into_iter().peekable();
        let (mut j, mut out, mut missing) = (Jitter::new(), Vec::new(), 0);
        for tick in 0..520u64 {
            while arrivals.peek().is_some_and(|a| a.0 <= tick * 20) {
                let (ms, p) = arrivals.next().unwrap();
                j.receive(p, t0 + Duration::from_millis(ms));
            }
            let (pcm, rate) = j.pull();
            assert_eq!(rate, CALL_RATE);
            if (100..500).contains(&tick) && j.concealed > 0 {
                missing += 1;
            }
            out.extend(pcm);
        }
        let step = out.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0f32, f32::max);
        println!("largest step {step:.4}, missing after 2 s: {missing}, delay {:.0} ms", j.target);
        // A 220 Hz tone at 0.5 moves by under 0.015 a sample; a drop to zero would be 0.5.
        assert!(step < 0.05, "step {step}");
        assert_eq!(missing, 0);
        assert!((80.0..=120.0).contains(&j.target), "{}", j.target);
    }

    /// A sender whose clock runs 0.5% fast (far more than real ones): the delay must not grow
    /// without end; quiet frames (pauses in speech) absorb the difference.
    #[test]
    fn fast_sender_keeps_delay_bounded() {
        let mut enc = OpusEncoder::new().unwrap();
        let t0 = Instant::now();
        let mut j = Jitter::new();
        let mut sent = 0u32;
        for tick in 0..3000u64 {
            // 60 s; the sender's 20 ms are our 19.9.
            while sent as f64 * 19.9 <= tick as f64 * 20.0 {
                let talking = (sent / 15).is_multiple_of(2);
                let pcm: Vec<f32> = (0..CALL_FRAME).map(|k| if talking { ((k as f32) * 0.05).sin() * 0.3 } else { 0.0 }).collect();
                let at = t0 + Duration::from_micros((sent as f64 * 19_900.0) as u64);
                j.receive(RtpAudio { seq: sent as u16, ts: sent * 960, ssrc: 3, pcmu: false, payload: enc.encode(&pcm).unwrap() }, at);
                sent += 1;
            }
            j.pull();
        }
        println!("buffered {:.0} ms, target {:.0} ms", j.buffered_ms(), j.target);
        assert!(j.buffered_ms() <= j.target + 60.0, "{} > {} + 60", j.buffered_ms(), j.target);
        assert!(j.target < 100.0, "{}", j.target);
    }

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
