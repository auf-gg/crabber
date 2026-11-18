//! macOS screen sharing through ScreenCaptureKit: the system picker chooses a display or a window
//! (no Screen Recording permission needed), a stream delivers its pictures as BGRA, and a second
//! stream on the same choice delivers what the other applications play.

#[cfg(target_os = "macos")]
pub use imp::{Capture, Pick, pick, start_audio, start_video};

/// Interleaved (`planar` false) or planar float samples, `frames` per channel, as mono. A planar
/// buffer holds its channels one after another, each `samples.len() / channels` long.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn downmix(samples: &[f32], frames: usize, channels: usize, planar: bool) -> Vec<f32> {
    let channels = channels.max(1);
    let plane = samples.len() / channels;
    let at = |i: usize, c: usize| if planar { c * plane + i } else { i * channels + c };
    if frames > plane {
        return Vec::new();
    }
    (0..frames).map(|i| (0..channels).map(|c| samples[at(i, c)]).sum::<f32>() / channels as f32).collect()
}

/// The capture size for a picture of `width`×`height` pixels: at most 2160 tall (a 5K display
/// would push 4× a 1440p picture through ffmpeg for nothing), even sides for the encoder.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn capture_size(width: f64, height: f64) -> (usize, usize) {
    let k = (2160. / height).min(1.);
    let even = |x: f64| ((x * k) as usize / 2 * 2).max(2);
    (even(width), even(height))
}

#[cfg(target_os = "macos")]
mod imp {
    use std::cell::OnceCell;
    use std::ptr::NonNull;
    use std::sync::mpsc::{self, Sender};
    use std::sync::Mutex;
    use std::time::Duration;

    use block2::RcBlock;
    use dispatch2::{DispatchQueue, DispatchRetained};
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, NSObject, ProtocolObject};
    use objc2::{AnyThread, DefinedClass, Message, define_class, msg_send};
    use objc2_core_audio_types::{kAudioFormatFlagIsFloat, kAudioFormatFlagIsNonInterleaved, kAudioFormatLinearPCM};
    use objc2_core_media::{CMAudioFormatDescriptionGetStreamBasicDescription, CMSampleBuffer, CMTime};
    use objc2_core_video::{
        CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetWidth,
        CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress, kCVReturnSuccess,
    };
    use objc2_foundation::{NSError, NSObjectProtocol};
    use objc2_screen_capture_kit::{
        SCContentFilter, SCContentSharingPicker, SCContentSharingPickerConfiguration, SCContentSharingPickerMode,
        SCContentSharingPickerObserver, SCStream, SCStreamConfiguration, SCStreamOutput, SCStreamOutputType,
    };

    use crate::videocall::RawFrame;

    /// What the user picked. Content filters never change once made, so any thread may use one.
    #[derive(Clone)]
    struct Filter(Retained<SCContentFilter>);
    unsafe impl Send for Filter {}

    /// The pick waiting for the picker's answer.
    static PENDING: Mutex<Option<Sender<Result<Filter, String>>>> = Mutex::new(None);

    /// What the running screen share shows, for the system audio that goes with it.
    static SHARED: Mutex<Option<Filter>> = Mutex::new(None);

    fn answer(result: Result<Filter, String>) {
        if let Some(tx) = PENDING.lock().unwrap().take() {
            let _ = tx.send(result);
        }
    }

    fn describe(error: &NSError) -> String {
        error.localizedDescription().to_string()
    }

    define_class!(
        // SAFETY: NSObject has no subclassing requirements and the class has no Drop.
        #[unsafe(super(NSObject))]
        #[name = "CrabberPickerObserver"]
        struct PickerObserver;

        unsafe impl NSObjectProtocol for PickerObserver {}

        unsafe impl SCContentSharingPickerObserver for PickerObserver {
            #[unsafe(method(contentSharingPicker:didCancelForStream:))]
            fn did_cancel(&self, _picker: &SCContentSharingPicker, _stream: Option<&SCStream>) {
                answer(Err("Screen sharing cancelled".into()));
            }

            #[unsafe(method(contentSharingPicker:didUpdateWithFilter:forStream:))]
            fn did_update(&self, _picker: &SCContentSharingPicker, filter: &SCContentFilter, _stream: Option<&SCStream>) {
                answer(Ok(Filter(filter.retain())));
            }

            #[unsafe(method(contentSharingPickerStartDidFailWithError:))]
            fn did_fail(&self, error: &NSError) {
                answer(Err(describe(error)));
            }
        }
    );

    impl PickerObserver {
        fn new() -> Retained<Self> {
            let this = Self::alloc().set_ivars(());
            unsafe { msg_send![super(this), init] }
        }
    }

    thread_local! {
        /// Lives on the main thread for good: the picker keeps only a weak reference to it.
        static OBSERVER: OnceCell<Retained<PickerObserver>> = const { OnceCell::new() };
    }

    /// Shows the system picker and waits until a display or a window is chosen (macOS 14 or
    /// newer). Must not run on the main thread, which shows the picker.
    fn pick_filter() -> Result<Filter, String> {
        if AnyClass::get(c"SCContentSharingPicker").is_none() {
            return Err("Screen sharing needs macOS 14 or newer".into());
        }
        let (tx, rx) = mpsc::channel();
        if let Some(old) = PENDING.lock().unwrap().replace(tx) {
            let _ = old.send(Err("Screen sharing cancelled".into()));
        }
        DispatchQueue::main().exec_async(|| unsafe {
            let picker = SCContentSharingPicker::sharedPicker();
            OBSERVER.with(|cell| {
                cell.get_or_init(|| {
                    let observer = PickerObserver::new();
                    picker.addObserver(ProtocolObject::from_ref(&*observer));
                    observer
                });
            });
            let config = SCContentSharingPickerConfiguration::new();
            config.setAllowedPickerModes(SCContentSharingPickerMode::SingleDisplay | SCContentSharingPickerMode::SingleWindow);
            picker.setDefaultConfiguration(&config);
            picker.setActive(true);
            picker.present();
        });
        rx.recv().map_err(|_| "Screen sharing cancelled".to_string())?
    }

    /// Opaque handle of [`pick`], for [`start_video`].
    pub struct Pick(Filter);

    pub fn pick() -> Result<Pick, String> {
        pick_filter().map(Pick)
    }

    type Handler = Mutex<Box<dyn FnMut(&CMSampleBuffer) + Send>>;

    define_class!(
        // SAFETY: NSObject has no subclassing requirements; define_class! drops the ivars.
        #[unsafe(super(NSObject))]
        #[name = "CrabberStreamOutput"]
        #[ivars = Handler]
        struct Output;

        unsafe impl NSObjectProtocol for Output {}

        unsafe impl SCStreamOutput for Output {
            #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
            fn did_output(&self, _stream: &SCStream, sample: &CMSampleBuffer, _kind: SCStreamOutputType) {
                if let Ok(mut handle) = self.ivars().lock() {
                    handle(sample);
                }
            }
        }
    );

    impl Output {
        fn new(handle: Box<dyn FnMut(&CMSampleBuffer) + Send>) -> Retained<Self> {
            let this = Self::alloc().set_ivars(Mutex::new(handle));
            unsafe { msg_send![super(this), init] }
        }
    }

    /// Running stream; dropping it stops it (and the system picker with a screen share).
    pub struct Capture {
        stream: Retained<SCStream>,
        output: Retained<Output>,
        kind: SCStreamOutputType,
        _queue: DispatchRetained<DispatchQueue>,
    }

    // SCStream may be stopped from any thread; the output is only called by the stream.
    unsafe impl Send for Capture {}

    impl Drop for Capture {
        fn drop(&mut self) {
            unsafe {
                let _ = self.stream.removeStreamOutput_type_error(ProtocolObject::from_ref(&*self.output), self.kind);
                self.stream.stopCaptureWithCompletionHandler(None);
            }
            if self.kind == SCStreamOutputType::Screen {
                *SHARED.lock().unwrap() = None;
                DispatchQueue::main().exec_async(|| unsafe { SCContentSharingPicker::sharedPicker().setActive(false) });
            }
        }
    }

    fn start(filter: &Filter, config: &SCStreamConfiguration, kind: SCStreamOutputType, handle: Box<dyn FnMut(&CMSampleBuffer) + Send>) -> Result<Capture, String> {
        let queue = DispatchQueue::new("crabber.screen-capture", None);
        unsafe {
            let stream = SCStream::initWithFilter_configuration_delegate(SCStream::alloc(), &filter.0, config, None);
            let output = Output::new(handle);
            stream.addStreamOutput_type_sampleHandlerQueue_error(ProtocolObject::from_ref(&*output), kind, Some(&queue)).map_err(|e| describe(&e))?;
            let capture = Capture { stream, output, kind, _queue: queue };
            let (tx, rx) = mpsc::channel();
            let done = RcBlock::new(move |error: *mut NSError| {
                let _ = tx.send(error.as_ref().map(describe));
            });
            capture.stream.startCaptureWithCompletionHandler(Some(&done));
            match rx.recv_timeout(Duration::from_secs(10)) {
                Ok(None) => Ok(capture),
                Ok(Some(error)) => Err(error),
                Err(_) => Err("Screen capture did not start".into()),
            }
        }
    }

    /// Captures the picked display or window at up to `fps` frames per second, cursor included;
    /// `on_frame` gets tightly packed BGRA on a capture thread.
    pub fn start_video(pick: &Pick, fps: u32, mut on_frame: impl FnMut(RawFrame) + Send + 'static) -> Result<Capture, String> {
        let filter = &pick.0;
        unsafe {
            let rect = filter.0.contentRect();
            let scale = f64::from(filter.0.pointPixelScale());
            let (width, height) = super::capture_size(rect.size.width * scale, rect.size.height * scale);
            let config = SCStreamConfiguration::new();
            config.setWidth(width);
            config.setHeight(height);
            config.setPixelFormat(u32::from_be_bytes(*b"BGRA"));
            config.setShowsCursor(true);
            config.setMinimumFrameInterval(CMTime::new(1, fps.max(1) as i32));
            config.setQueueDepth(5);
            let capture = start(filter, &config, SCStreamOutputType::Screen, Box::new(move |sample| {
                // Frames of a still picture come without an image.
                if let Some(image) = sample.image_buffer()
                    && let Some(frame) = bgra(&image)
                {
                    on_frame(frame);
                }
            }))?;
            *SHARED.lock().unwrap() = Some(filter.clone());
            Ok(capture)
        }
    }

    /// Tightly packed copy of a BGRA pixel buffer.
    fn bgra(pixels: &CVPixelBuffer) -> Option<RawFrame> {
        unsafe {
            if CVPixelBufferLockBaseAddress(pixels, CVPixelBufferLockFlags::ReadOnly) != kCVReturnSuccess {
                return None;
            }
            let (width, height, stride) = (CVPixelBufferGetWidth(pixels), CVPixelBufferGetHeight(pixels), CVPixelBufferGetBytesPerRow(pixels));
            let base = CVPixelBufferGetBaseAddress(pixels) as *const u8;
            let row = width * 4;
            let frame = (!base.is_null() && width > 0 && height > 0 && stride >= row).then(|| {
                let source = std::slice::from_raw_parts(base, stride * (height - 1) + row);
                let mut data = Vec::with_capacity(row * height);
                for y in 0..height {
                    data.extend_from_slice(&source[y * stride..y * stride + row]);
                }
                RawFrame { width: width as u32, height: height as u32, data }
            });
            CVPixelBufferUnlockBaseAddress(pixels, CVPixelBufferLockFlags::ReadOnly);
            frame
        }
    }

    /// What the other applications play on the shared display or in the shared window, as 20 ms
    /// frames of 48 kHz mono (960 samples). Only while a screen is shared.
    pub fn start_audio(mut on_frame: Box<dyn FnMut(Vec<f32>) + Send>) -> Result<Capture, String> {
        let filter = SHARED.lock().unwrap().clone().ok_or("No screen is shared")?;
        unsafe {
            let config = SCStreamConfiguration::new();
            config.setCapturesAudio(true);
            config.setExcludesCurrentProcessAudio(true);
            config.setSampleRate(48_000);
            config.setChannelCount(2);
            // A stream always has a picture too: keep it tiny and rare, nothing takes it.
            config.setWidth(2);
            config.setHeight(2);
            config.setMinimumFrameInterval(CMTime::new(1, 1));
            let mut pending = Vec::with_capacity(1920);
            start(&filter, &config, SCStreamOutputType::Audio, Box::new(move |sample| {
                let Some(mono) = mono(sample) else { return };
                pending.extend(mono);
                while pending.len() >= 960 {
                    on_frame(pending.drain(..960).collect());
                }
            }))
        }
    }

    /// The samples of an audio buffer (32-bit float at 48 kHz, as configured) as mono.
    unsafe fn mono(sample: &CMSampleBuffer) -> Option<Vec<f32>> {
        unsafe {
            let format = sample.format_description()?;
            let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(&format).as_ref()?;
            if asbd.mFormatID != kAudioFormatLinearPCM || asbd.mFormatFlags & kAudioFormatFlagIsFloat == 0 || asbd.mBitsPerChannel != 32 || asbd.mSampleRate != 48_000. {
                return None;
            }
            let frames = usize::try_from(sample.num_samples()).ok()?;
            let block = sample.data_buffer()?;
            let len = block.data_length() / 4 * 4;
            let mut bytes = vec![0u8; len];
            if len == 0 || block.copy_data_bytes(0, len, NonNull::new(bytes.as_mut_ptr().cast())?) != 0 {
                return None;
            }
            let samples: Vec<f32> = bytes.as_chunks::<4>().0.iter().map(|b| f32::from_ne_bytes(*b)).collect();
            let planar = asbd.mFormatFlags & kAudioFormatFlagIsNonInterleaved != 0;
            Some(super::downmix(&samples, frames, asbd.mChannelsPerFrame as usize, planar))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmix_planar_and_interleaved() {
        assert_eq!(downmix(&[1., 3., 0.5, -0.5], 2, 2, false), [2., 0.]);
        assert_eq!(downmix(&[1., 0.5, 3., -0.5], 2, 2, true), [2., 0.]);
        // Planes padded past the frames: the second channel starts after the padding.
        assert_eq!(downmix(&[1., 1., 0., 3., 3., 0.], 2, 2, true), [2., 2.]);
        assert_eq!(downmix(&[0.25, 0.5], 2, 1, false), [0.25, 0.5]);
        assert!(downmix(&[1., 2.], 2, 2, false).is_empty());
    }

    #[test]
    fn capture_sizes() {
        // A Retina MacBook stays as it is; a 5K display comes down to 2160 tall.
        assert_eq!(capture_size(3024., 1964.), (3024, 1964));
        assert_eq!(capture_size(5120., 2880.), (3840, 2160));
        assert_eq!(capture_size(1001., 701.), (1000, 700));
        assert_eq!(capture_size(0., 0.), (2, 2));
    }
}
