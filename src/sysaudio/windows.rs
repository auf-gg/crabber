//! Process-excluding WASAPI loopback, so a peer never hears their own playback.

#[cfg(windows)]
pub use imp::start;

fn append_mono(pending: &mut Vec<f32>, stereo: Option<&[f32]>, frames: usize) {
    if let Some(samples) = stereo {
        pending.extend(samples.chunks_exact(2).map(|pair| (pair[0] + pair[1]) * 0.5));
    } else {
        pending.resize(pending.len() + frames, 0.0);
    }
}

#[cfg(windows)]
mod imp {
    use std::sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc};
    use std::thread::{self, JoinHandle};
    use std::time::Duration;
    use windows::core::{HRESULT, Interface, Ref, implement};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::Media::Audio::{ActivateAudioInterfaceAsync, AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK, IActivateAudioInterfaceAsyncOperation, IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl, IAudioCaptureClient, IAudioClient, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, WAVEFORMATEX};
    use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
    use windows::Win32::System::Com::{BLOB, COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
    use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
    use windows::Win32::System::Threading::{CreateEventW, GetCurrentProcessId, WaitForSingleObject};
    use windows::Win32::System::Variant::VT_BLOB;

    #[implement(IActivateAudioInterfaceCompletionHandler)]
    struct Completion(mpsc::Sender<()>);

    #[allow(non_snake_case)]
    impl IActivateAudioInterfaceCompletionHandler_Impl for Completion_Impl {
        fn ActivateCompleted(&self, _: Ref<'_, IActivateAudioInterfaceAsyncOperation>) -> windows::core::Result<()> {
            let _ = self.0.send(());
            Ok(())
        }
    }

    struct Com;
    impl Drop for Com {
        fn drop(&mut self) { unsafe { CoUninitialize(); } }
    }

    struct Event(HANDLE);
    impl Drop for Event {
        fn drop(&mut self) { let _ = unsafe { CloseHandle(self.0) }; }
    }

    struct Capture {
        client: IAudioClient,
        packets: IAudioCaptureClient,
        event: Event,
    }

    impl Drop for Capture {
        fn drop(&mut self) { let _ = unsafe { self.client.Stop() }; }
    }

    impl Capture {
        fn new() -> Result<Self, String> {
            unsafe {
                let (send, completed) = mpsc::channel();
                let handler: IActivateAudioInterfaceCompletionHandler = Completion(send).into();
                let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
                    ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
                    Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
                        ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                            TargetProcessId: GetCurrentProcessId(),
                            ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
                        },
                    },
                };
                let mut variant = PROPVARIANT::default();
                (*variant.Anonymous.Anonymous).vt = VT_BLOB;
                (*variant.Anonymous.Anonymous).Anonymous.blob = BLOB {
                    cbSize: std::mem::size_of_val(&params) as u32,
                    pBlobData: (&mut params as *mut AUDIOCLIENT_ACTIVATION_PARAMS).cast(),
                };
                let unsupported = || "system audio requires Windows process loopback support (build 20348 or newer)".to_string();
                let operation = ActivateAudioInterfaceAsync(VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK, &IAudioClient::IID, Some(&variant), &handler)
                    .map_err(|_| unsupported())?;
                completed.recv_timeout(Duration::from_secs(5)).map_err(|_| "system audio activation timed out".to_string())?;
                let mut result = HRESULT(0);
                let mut activated = None;
                operation.GetActivateResult(&mut result, &mut activated).map_err(|_| unsupported())?;
                result.ok().map_err(|_| unsupported())?;
                let client: IAudioClient = activated.ok_or_else(unsupported)?.cast().map_err(|_| unsupported())?;
                let format = WAVEFORMATEX {
                    wFormatTag: WAVE_FORMAT_IEEE_FLOAT as u16,
                    nChannels: 2,
                    nSamplesPerSec: 48_000,
                    nAvgBytesPerSec: 48_000 * 8,
                    nBlockAlign: 8,
                    wBitsPerSample: 32,
                    cbSize: 0,
                };
                client.Initialize(AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                    200_000, 0, &format, None).map_err(|e| format!("could not initialize system audio: {e}"))?;
                let event = Event(CreateEventW(None, false, false, None).map_err(|e| format!("could not create system audio event: {e}"))?);
                client.SetEventHandle(event.0).map_err(|e| format!("could not set system audio event: {e}"))?;
                let packets = client.GetService().map_err(|e| format!("could not open system audio capture: {e}"))?;
                let capture = Self { client, packets, event };
                capture.client.Start().map_err(|e| format!("could not start system audio capture: {e}"))?;
                Ok(capture)
            }
        }

        fn run(&self, stop: &AtomicBool, on_frame: &mut dyn FnMut(Vec<f32>)) -> Result<(), String> {
            let mut pending = Vec::with_capacity(1920);
            while !stop.load(Ordering::Relaxed) {
                let wait = unsafe { WaitForSingleObject(self.event.0, 100) };
                if wait != WAIT_OBJECT_0 && wait != WAIT_TIMEOUT {
                    return Err("system audio event wait failed".into());
                }
                while !stop.load(Ordering::Relaxed) && unsafe { self.packets.GetNextPacketSize() }.map_err(|e| format!("system audio packet failed: {e}"))? > 0 {
                    let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0, 0);
                    unsafe { self.packets.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }.map_err(|e| format!("system audio buffer failed: {e}"))?;
                    let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                    if !silent && frames != 0 && data.is_null() {
                        let _ = unsafe { self.packets.ReleaseBuffer(frames) };
                        return Err("system audio returned an empty buffer".into());
                    }
                    let stereo = if silent || frames == 0 { None } else {
                        Some(unsafe { std::slice::from_raw_parts(data.cast::<f32>(), frames as usize * 2) })
                    };
                    super::append_mono(&mut pending, stereo, frames as usize);
                    unsafe { self.packets.ReleaseBuffer(frames) }.map_err(|e| format!("system audio buffer release failed: {e}"))?;
                    while pending.len() >= 960 && !stop.load(Ordering::Relaxed) {
                        on_frame(pending.drain(..960).collect());
                    }
                }
            }
            Ok(())
        }
    }

    struct Handle { stop: Arc<AtomicBool>, thread: Option<JoinHandle<()>> }

    impl Drop for Handle {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                if thread.thread().id() != thread::current().id() { let _ = thread.join(); }
            }
        }
    }

    /// Starts 48 kHz mono capture excluding this process and its children. Drop to stop.
    pub fn start(mut on_frame: Box<dyn FnMut(Vec<f32>) + Send>) -> Result<impl Send + 'static, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let (ready, started) = mpsc::channel();
        let thread = thread::Builder::new().name("system-audio".into()).spawn(move || {
            let mut run = || -> Result<(), String> {
                unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok().map_err(|e| format!("could not initialize system audio COM: {e}"))?;
                let _com = Com;
                let capture = Capture::new()?;
                if ready.send(Ok(())).is_ok() { capture.run(&stopped, &mut on_frame)?; }
                Ok(())
            };
            if let Err(error) = run() {
                if ready.send(Err(error.clone())).is_err() { eprintln!("system audio: {error}"); }
            }
        }).map_err(|e| format!("could not start system audio thread: {e}"))?;
        let handle = Handle { stop, thread: Some(thread) };
        started.recv().map_err(|_| "system audio capture thread stopped".to_string())??;
        Ok(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::append_mono;

    #[test]
    fn downmix_and_silence_preserve_pending_samples() {
        let mut pending = vec![0.25];
        append_mono(&mut pending, Some(&[1.0, -1.0, 0.5, 0.25]), 2);
        append_mono(&mut pending, None, 2);
        assert_eq!(pending, [0.25, 0.0, 0.375, 0.0, 0.0]);
    }
}
