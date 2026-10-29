//! Call audio: microphone -> 8 kHz G.711 µ-law frames, and µ-law RTP payloads -> speaker.
//! G.711 needs no native codec library and every WebRTC stack (Conversations, Dino) offers it.
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample as _, SampleFormat, SizedSample, StreamConfig};

pub const RATE: u32 = 8000;
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

struct Playback {
    buf: VecDeque<f32>,
    up: Up,
    rate: u32,
}

pub struct Audio {
    pub muted: Arc<AtomicBool>,
    playback: Arc<Mutex<Playback>>,
    stop: std::sync::mpsc::Sender<()>,
}

impl Drop for Audio {
    fn drop(&mut self) {
        let _ = self.stop.send(());
    }
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

/// Opens the default microphone and speaker. `on_frame` receives 160-byte µ-law frames.
/// cpal streams are not Send, so they live on their own thread until `Audio` is dropped.
pub fn start(on_frame: impl Fn(Vec<u8>) + Send + 'static) -> Result<Audio, String> {
    let muted = Arc::new(AtomicBool::new(false));
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<Arc<Mutex<Playback>>, String>>();
    let m = muted.clone();
    std::thread::spawn(move || {
        let streams = (|| {
            let host = cpal::default_host();
            let output = host.default_output_device().ok_or("нет устройства вывода звука")?;
            let out_cfg = output.default_output_config().map_err(|e| e.to_string())?;
            let out_rate = out_cfg.sample_rate();
            let playback = Arc::new(Mutex::new(Playback {
                buf: VecDeque::new(),
                up: Up { step: RATE as f64 / out_rate as f64, frac: 0.0, last: 0.0 },
                rate: out_rate,
            }));
            let out_stream = match out_cfg.sample_format() {
                SampleFormat::I16 => build_output::<i16>(&output, out_cfg.config(), playback.clone()),
                SampleFormat::U16 => build_output::<u16>(&output, out_cfg.config(), playback.clone()),
                _ => build_output::<f32>(&output, out_cfg.config(), playback.clone()),
            }?;

            let input = host.default_input_device().ok_or("нет микрофона")?;
            let in_cfg = input.default_input_config().map_err(|e| e.to_string())?;
            let in_stream = match in_cfg.sample_format() {
                SampleFormat::I16 => build_input::<i16>(&input, in_cfg.config(), m, on_frame),
                SampleFormat::U16 => build_input::<u16>(&input, in_cfg.config(), m, on_frame),
                _ => build_input::<f32>(&input, in_cfg.config(), m, on_frame),
            }?;
            out_stream.play().map_err(|e| e.to_string())?;
            in_stream.play().map_err(|e| e.to_string())?;
            Ok::<_, String>((in_stream, out_stream, playback))
        })();
        match streams {
            Ok((in_stream, out_stream, playback)) => {
                let _ = init_tx.send(Ok(playback));
                let _ = stop_rx.recv();
                drop((in_stream, out_stream));
            }
            Err(e) => {
                let _ = init_tx.send(Err(e));
            }
        }
    });
    let playback = init_rx.recv().map_err(|_| "аудиопоток завершился".to_string())??;
    Ok(Audio { muted, playback, stop: stop_tx })
}

/// Ringing played on the default speaker until dropped.
pub struct Tone(#[allow(dead_code)] std::sync::mpsc::Sender<()>);

/// `incoming`: the ring for a call to us; otherwise the ringback heard while calling out.
pub fn tone(incoming: bool) -> Tone {
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let res = (|| {
            let output = cpal::default_host().default_output_device().ok_or("нет устройства вывода звука")?;
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
    let mut down = Down { ratio: config.sample_rate as f64 / RATE as f64, pos: 0.0, acc: 0.0, n: 0 };
    let mut pcm: Vec<f32> = Vec::with_capacity(FRAME * 2);
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                for frame in data.chunks(channels) {
                    let mono = frame.iter().map(|&s| f32::from_sample(s)).sum::<f32>() / channels as f32;
                    down.push(mono, &mut pcm);
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
