//! System audio for screen sharing: what the other applications play, never our own playback
//! (the peer would hear themselves back).
#[cfg(target_os = "linux")]
mod linux;
#[cfg(windows)]
mod windows;

/// Running capture; dropping it stops it.
pub struct SystemAudio(#[allow(dead_code)] Box<dyn Send>);

/// Starts capturing; `on_frame` gets 20 ms frames of 48 kHz mono samples (960 each).
pub fn start(on_frame: impl FnMut(Vec<f32>) + Send + 'static) -> Result<SystemAudio, String> {
    let on_frame: Box<dyn FnMut(Vec<f32>) + Send> = Box::new(on_frame);
    #[cfg(target_os = "linux")]
    return linux::start(on_frame).map(|h| SystemAudio(Box::new(h)));
    #[cfg(windows)]
    return windows::start(on_frame).map(|h| SystemAudio(Box::new(h)));
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        drop(on_frame);
        Err("system audio capture is not supported here".into())
    }
}
