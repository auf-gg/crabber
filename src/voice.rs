//! Voice messages: microphone -> Ogg Opus file (16 kHz mono, like Conversations), and Ogg Opus -> speaker.
use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample as _, SampleFormat, SizedSample, StreamConfig};
use ogg::{PacketReader, PacketWriteEndInfo, PacketWriter};
use unsafe_libopus as opus;

use crate::audio::{Down, Up};

const RATE: u32 = 16000;
/// 20 ms at 16 kHz.
const FRAME: usize = 320;
/// Ogg Opus granule positions always count 48 kHz samples.
const GRANULE_PER_FRAME: u64 = 960;
/// Encoder lookahead at 16 kHz in 48 kHz samples (RFC 7845 pre-skip).
const PRE_SKIP: u16 = 312;
const SERIAL: u32 = 0x5643_5242;

/// Records the default microphone until `finish`/drop. cpal streams are not Send: they live on their own thread.
pub struct Recorder {
    pcm: Arc<Mutex<Vec<i16>>>,
    /// Peak of the latest input block (f32 bits), for the level meter.
    level: Arc<AtomicU32>,
    stop: std::sync::mpsc::Sender<()>,
}

impl Recorder {
    pub fn start() -> Result<Self, String> {
        let pcm = Arc::new(Mutex::new(Vec::new()));
        let level = Arc::new(AtomicU32::new(0));
        let lv = level.clone();
        let (stop, stop_rx) = std::sync::mpsc::channel::<()>();
        let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let buf = pcm.clone();
        std::thread::spawn(move || {
            let stream = (|| {
                let input = cpal::default_host().default_input_device().ok_or("нет микрофона")?;
                let cfg = input.default_input_config().map_err(|e| e.to_string())?;
                let stream = match cfg.sample_format() {
                    SampleFormat::I16 => record::<i16>(&input, cfg.config(), buf, lv),
                    SampleFormat::U16 => record::<u16>(&input, cfg.config(), buf, lv),
                    _ => record::<f32>(&input, cfg.config(), buf, lv),
                }?;
                stream.play().map_err(|e| e.to_string())?;
                Ok::<_, String>(stream)
            })();
            match stream {
                Ok(stream) => {
                    let _ = init_tx.send(Ok(()));
                    let _ = stop_rx.recv();
                    drop(stream);
                }
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                }
            }
        });
        init_rx.recv().map_err(|_| "аудиопоток завершился".to_string())??;
        Ok(Self { pcm, level, stop })
    }

    /// 0.0 ..= 1.0, the loudest sample of the last few milliseconds.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    /// Stops recording and returns 16 kHz mono samples.
    pub fn finish(self) -> Vec<i16> {
        let _ = self.stop.send(());
        std::mem::take(&mut *self.pcm.lock().unwrap())
    }
}

fn record<T>(device: &cpal::Device, config: StreamConfig, pcm: Arc<Mutex<Vec<i16>>>, level: Arc<AtomicU32>) -> Result<cpal::Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    let mut down = Down { ratio: config.sample_rate as f64 / RATE as f64, pos: 0.0, acc: 0.0, n: 0 };
    let mut tmp = Vec::new();
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                for frame in data.chunks(channels) {
                    down.push(frame.iter().map(|&s| f32::from_sample(s)).sum::<f32>() / channels as f32, &mut tmp);
                }
                level.store(tmp.iter().fold(0f32, |m, x| m.max(x.abs())).to_bits(), Ordering::Relaxed);
                pcm.lock().unwrap().extend(tmp.drain(..).map(|x| (x.clamp(-1.0, 1.0) * 32767.0) as i16));
            },
            |e| eprintln!("voice input error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())
}

/// 16 kHz mono PCM -> Ogg Opus file bytes (RFC 7845).
pub fn encode(pcm: &[i16]) -> Result<Vec<u8>, String> {
    if pcm.is_empty() {
        return Err("пустая запись".into());
    }
    let mut out = Vec::new();
    let mut w = PacketWriter::new(&mut out);
    let mut head = b"OpusHead".to_vec();
    head.extend([1, 1]); // version, mono
    head.extend(PRE_SKIP.to_le_bytes());
    head.extend(RATE.to_le_bytes());
    head.extend([0, 0, 0]); // output gain, mapping family 0
    let mut tags = b"OpusTags".to_vec();
    tags.extend(7u32.to_le_bytes());
    tags.extend(b"crabber");
    tags.extend(0u32.to_le_bytes());
    let io = |e: std::io::Error| e.to_string();
    w.write_packet(head, SERIAL, PacketWriteEndInfo::EndPage, 0).map_err(io)?;
    w.write_packet(tags, SERIAL, PacketWriteEndInfo::EndPage, 0).map_err(io)?;

    let mut err = 0;
    let enc = unsafe { opus::opus_encoder_create(RATE as i32, 1, opus::OPUS_APPLICATION_VOIP, &mut err) };
    if enc.is_null() || err != opus::OPUS_OK {
        return Err(format!("opus: ошибка {err}"));
    }
    let frames = pcm.len().div_ceil(FRAME);
    // The last page's granule trims the zero padding of the final frame.
    let end = pcm.len() as u64 * 3 + PRE_SKIP as u64;
    let mut res = Ok(());
    for (i, chunk) in pcm.chunks(FRAME).enumerate() {
        let mut frame = [0i16; FRAME];
        frame[..chunk.len()].copy_from_slice(chunk);
        let mut packet = [0u8; 1500];
        let n = unsafe { opus::opus_encode(enc, frame.as_ptr(), FRAME as i32, packet.as_mut_ptr(), packet.len() as i32) };
        if n < 0 {
            res = Err(format!("opus: ошибка {n}"));
            break;
        }
        let last = i + 1 == frames;
        let info = if last {
            PacketWriteEndInfo::EndStream
        } else if (i + 1) % 50 == 0 {
            PacketWriteEndInfo::EndPage // a page per second
        } else {
            PacketWriteEndInfo::NormalPacket
        };
        let granule = ((i as u64 + 1) * GRANULE_PER_FRAME).min(end);
        if let Err(e) = w.write_packet(packet[..n as usize].to_vec(), SERIAL, info, granule) {
            res = Err(e.to_string());
            break;
        }
    }
    unsafe { opus::opus_encoder_destroy(enc) };
    drop(w);
    res.map(|_| out)
}

/// Ogg Opus file -> 48 kHz mono samples.
pub fn decode(bytes: &[u8]) -> Result<Vec<f32>, String> {
    let mut r = PacketReader::new(Cursor::new(bytes));
    let next = |r: &mut PacketReader<Cursor<&[u8]>>| r.read_packet().map_err(|e| e.to_string());
    let head = next(&mut r)?.ok_or("пустой файл")?.data;
    if head.len() < 19 || !head.starts_with(b"OpusHead") {
        return Err("не Opus".into());
    }
    let channels = head[9].clamp(1, 2) as usize;
    let pre_skip = u16::from_le_bytes([head[10], head[11]]) as usize;
    next(&mut r)?; // OpusTags
    let mut err = 0;
    let dec = unsafe { opus::opus_decoder_create(48000, channels as i32, &mut err) };
    if dec.is_null() || err != opus::OPUS_OK {
        return Err(format!("opus: ошибка {err}"));
    }
    let mut out = Vec::new();
    let mut buf = vec![0i16; 5760 * channels]; // 120 ms, the longest Opus packet
    while let Ok(Some(p)) = r.read_packet() {
        let n = unsafe { opus::opus_decode(dec, p.data.as_ptr(), p.data.len() as i32, buf.as_mut_ptr(), 5760, 0) };
        if n > 0 {
            out.extend(buf[..n as usize * channels].chunks(channels).map(|f| f.iter().map(|&s| s as f32).sum::<f32>() / channels as f32 / 32768.0));
        }
    }
    unsafe { opus::opus_decoder_destroy(dec) };
    out.drain(..pre_skip.min(out.len()));
    Ok(out)
}

/// Length in seconds from the last page's granule, without decoding.
pub fn duration(bytes: &[u8]) -> Option<f32> {
    let head = bytes.windows(8).position(|w| w == b"OpusHead")?;
    let pre_skip = u16::from_le_bytes([*bytes.get(head + 10)?, *bytes.get(head + 11)?]) as u64;
    let last = bytes.windows(4).rposition(|w| w == b"OggS")?;
    let granule = u64::from_le_bytes(bytes.get(last + 6..last + 14)?.try_into().ok()?);
    Some(granule.saturating_sub(pre_skip) as f32 / 48000.0)
}

/// Plays 48 kHz mono samples on the default speaker until finished or dropped.
pub struct Player {
    pos: Arc<AtomicUsize>,
    len: usize,
    done: Arc<AtomicBool>,
    _stop: std::sync::mpsc::Sender<()>,
}

impl Player {
    pub fn start(samples: Vec<f32>) -> Result<Self, String> {
        let (stop, stop_rx) = std::sync::mpsc::channel::<()>();
        let (init_tx, init_rx) = std::sync::mpsc::channel::<Result<(Arc<AtomicUsize>, usize, Arc<AtomicBool>), String>>();
        std::thread::spawn(move || {
            let res = (|| {
                let output = cpal::default_host().default_output_device().ok_or("нет устройства вывода звука")?;
                let cfg = output.default_output_config().map_err(|e| e.to_string())?;
                let mut up = Up { step: 48000.0 / cfg.sample_rate() as f64, frac: 0.0, last: 0.0 };
                let mut buf = VecDeque::new();
                samples.iter().for_each(|&x| up.push(x, &mut buf));
                let pcm: Arc<Vec<f32>> = Arc::new(buf.into());
                let (pos, done) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicBool::new(false)));
                let stream = match cfg.sample_format() {
                    SampleFormat::I16 => play::<i16>(&output, cfg.config(), pcm.clone(), pos.clone(), done.clone()),
                    SampleFormat::U16 => play::<u16>(&output, cfg.config(), pcm.clone(), pos.clone(), done.clone()),
                    _ => play::<f32>(&output, cfg.config(), pcm.clone(), pos.clone(), done.clone()),
                }?;
                stream.play().map_err(|e| e.to_string())?;
                Ok::<_, String>((stream, pos, pcm.len(), done))
            })();
            match res {
                Ok((stream, pos, len, done)) => {
                    let _ = init_tx.send(Ok((pos, len, done)));
                    let _ = stop_rx.recv();
                    drop(stream);
                }
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                }
            }
        });
        let (pos, len, done) = init_rx.recv().map_err(|_| "аудиопоток завершился".to_string())??;
        Ok(Self { pos, len, done, _stop: stop })
    }

    /// 0.0 ..= 1.0
    pub fn progress(&self) -> f32 {
        if self.len == 0 { 1.0 } else { self.pos.load(Ordering::Relaxed) as f32 / self.len as f32 }
    }

    pub fn finished(&self) -> bool {
        self.done.load(Ordering::Relaxed)
    }
}

fn play<T>(device: &cpal::Device, config: StreamConfig, pcm: Arc<Vec<f32>>, pos: Arc<AtomicUsize>, done: Arc<AtomicBool>) -> Result<cpal::Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                let mut i = pos.load(Ordering::Relaxed);
                for frame in data.chunks_mut(channels) {
                    let v = T::from_sample(pcm.get(i).copied().unwrap_or(0.0));
                    frame.iter_mut().for_each(|s| *s = v);
                    i += 1;
                }
                pos.store(i.min(pcm.len()), Ordering::Relaxed);
                if i >= pcm.len() {
                    done.store(true, Ordering::Relaxed);
                }
            },
            |e| eprintln!("voice output error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    #[test]
    fn opus_roundtrip() {
        // 1 s of a 440 Hz tone at 16 kHz.
        let pcm: Vec<i16> = (0..16000).map(|i| ((i as f32 * 440.0 * std::f32::consts::TAU / 16000.0).sin() * 8000.0) as i16).collect();
        let file = super::encode(&pcm).unwrap();
        assert!(file.starts_with(b"OggS"));
        assert!(file.len() < 16000, "compressed: {} bytes", file.len());
        let len = super::duration(&file).unwrap();
        assert!((len - 1.0).abs() < 0.01, "{len}");
        let out = super::decode(&file).unwrap();
        assert!((out.len() as i32 - 48000).abs() < 960, "{}", out.len());
        let energy = out[4800..43200].iter().map(|x| x * x).sum::<f32>() / 38400.0;
        assert!(energy > 0.01, "the tone survives: {energy}");
        assert!(super::encode(&[]).is_err());
    }
}
