//! Wayland screen capture through the desktop portal and PipeWire.
use ashpd::desktop::{PersistMode, Session, screencast::{CursorMode, Screencast as Portal, SelectSourcesOptions, SourceType}};
use futures::executor::block_on;
use pipewire::{self as pw, spa};
use spa::param::{ParamType, format::{FormatProperties, MediaSubtype, MediaType}, video::{VideoFormat, VideoInfoRaw}};
use spa::pod::{Pod, Value, object, property};
use std::os::fd::OwnedFd;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

/// One captured frame, tightly packed BGRA (stride == width*4).
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

/// Running capture; dropping it stops PipeWire and closes the portal session.
pub struct Screencast {
    stop: Option<pw::channel::Sender<()>>,
    thread: Option<JoinHandle<()>>,
    session: Session<Portal>,
}

impl Drop for Screencast {
    fn drop(&mut self) {
        if let Some(stop) = &self.stop { let _ = stop.send(()); }
        if let Some(thread) = self.thread.take() { let _ = thread.join(); }
        let _ = block_on(self.session.close());
    }
}

/// True when running under Wayland (WAYLAND_DISPLAY set).
pub fn wayland() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
}

/// Opens the portal picker (monitor or window, cursor embedded), then delivers frames on a
/// background thread. Blocks the calling thread until the user picks or cancels.
/// Err(String) if the portal is missing, the user cancelled, or PipeWire fails.
pub fn start(on_frame: impl FnMut(Frame) + Send + 'static) -> Result<Screencast, String> {
    let portal = block_on(Portal::new()).map_err(portal_error)?;
    let session = block_on(portal.create_session(Default::default())).map_err(portal_error)?;
    let mut capture = Screencast { stop: None, thread: None, session };
    let (fd, node) = block_on(async {
        portal.select_sources(&capture.session, SelectSourcesOptions::default()
            .set_sources(SourceType::Monitor | SourceType::Window)
            .set_multiple(false)
            .set_cursor_mode(CursorMode::Embedded)
            .set_persist_mode(PersistMode::DoNot)).await?.response()?;
        let streams = portal.start(&capture.session, None, Default::default()).await?.response()?;
        let node = streams.streams().first().map(|stream| stream.pipe_wire_node_id())
            .ok_or(ashpd::Error::NoResponse)?;
        let fd = portal.open_pipe_wire_remote(&capture.session, Default::default()).await?;
        Ok::<_, ashpd::Error>((fd, node))
    }).map_err(portal_error)?;
    let (stop, receiver) = pw::channel::channel();
    capture.stop = Some(stop);
    let (ready, result) = mpsc::channel();
    capture.thread = Some(std::thread::Builder::new().name("screencast".into()).spawn(move || {
        let error = run(fd, node, receiver, ready.clone(), on_frame).err()
            .unwrap_or_else(|| "PipeWire stream stopped".into());
        let _ = ready.send(Err(error));
    }).map_err(|_| "Could not start screen capture thread")?);
    result.recv_timeout(Duration::from_secs(15)).map_err(|_| "PipeWire connection timed out")??;
    Ok(capture)
}

fn portal_error(error: ashpd::Error) -> String {
    match error {
        ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)
        | ashpd::Error::Portal(ashpd::PortalError::Cancelled(_)) => "Screen sharing cancelled",
        _ => "Screen sharing portal unavailable",
    }.into()
}

fn pod(value: Value) -> Result<Vec<u8>, String> {
    spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &value)
        .map(|(cursor, _)| cursor.into_inner())
        .map_err(|_| "Could not configure PipeWire".into())
}

fn run(fd: OwnedFd, node: u32, stop: pw::channel::Receiver<()>, ready: mpsc::Sender<Result<(), String>>,
    mut on_frame: impl FnMut(Frame) + 'static) -> Result<(), String> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|_| "Could not create PipeWire loop")?;
    let _stop = stop.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|_| "Could not create PipeWire context")?;
    let core = context.connect_fd_rc(fd, None).map_err(|_| "Could not connect to PipeWire")?;
    let _core_listener = core.add_listener_local().error({
        let mainloop = mainloop.clone();
        let ready = ready.clone();
        move |_, _, _, _| {
            let _ = ready.send(Err("PipeWire connection failed".into()));
            mainloop.quit();
        }
    }).register();
    let stream = pw::stream::StreamBox::new(&core, "crabber-screencast", pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Screen",
    }).map_err(|_| "Could not create PipeWire stream")?;
    // Only CPU-readable memory; no DMA-BUF formats or modifiers are offered.
    let buffers = pod(Value::Object(spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties: vec![spa::pod::Property::new(spa::sys::SPA_PARAM_BUFFERS_dataType,
            Value::Int((1 << spa::buffer::DataType::MemPtr.as_raw()) | (1 << spa::buffer::DataType::MemFd.as_raw())))],
    }))?;
    let _listener = stream.add_local_listener_with_user_data(VideoInfoRaw::new())
        .state_changed({
            let mainloop = mainloop.clone();
            let ready = ready.clone();
            move |_, _, _, state| match state {
                pw::stream::StreamState::Streaming => { let _ = ready.send(Ok(())); }
                pw::stream::StreamState::Error(_) | pw::stream::StreamState::Unconnected => {
                    let _ = ready.send(Err("PipeWire stream failed".into()));
                    mainloop.quit();
                }
                _ => {}
            }
        })
        .param_changed({
            let mainloop = mainloop.clone();
            let ready = ready.clone();
            move |stream, format, id, param| {
                if id != ParamType::Format.as_raw() { return; }
                *format = VideoInfoRaw::new();
                let Some(param) = param else { return; };
                let result = (|| {
                    if spa::param::format_utils::parse_format(param).ok() != Some((MediaType::Video, MediaSubtype::Raw)) {
                        return Err("Unsupported PipeWire format");
                    }
                    format.parse(param).map_err(|_| "Invalid PipeWire format")?;
                    if !matches!(format.format(), VideoFormat::BGRx | VideoFormat::BGRA | VideoFormat::RGBx | VideoFormat::RGBA) {
                        return Err("Unsupported PipeWire pixel format");
                    }
                    let buffers = Pod::from_bytes(&buffers).ok_or("Invalid PipeWire buffer parameters")?;
                    stream.update_params(&mut [buffers]).map_err(|_| "Could not configure PipeWire buffers")
                })();
                if let Err(error) = result {
                    let _ = ready.send(Err(error.into()));
                    mainloop.quit();
                }
            }
        })
        .process(move |stream, format| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return; };
            let Some(data) = buffer.datas_mut().first_mut() else { return; };
            if data.as_raw().chunk.is_null() || !matches!(data.type_(), spa::buffer::DataType::MemPtr | spa::buffer::DataType::MemFd) {
                return;
            }
            let chunk = data.chunk();
            // SPA_CHUNK_FLAG_EMPTY (bit 1) is not exposed by the bindings.
            if chunk.size() == 0 || chunk.flags().bits() & 3 != 0 { return; }
            let (offset, size, stride) = (chunk.offset() as usize, chunk.size() as usize, chunk.stride());
            let Some(bytes) = data.data().filter(|bytes| !bytes.is_empty()) else { return; };
            let offset = offset % bytes.len();
            let Some(source) = offset.checked_add(size).and_then(|end| bytes.get(offset..end)) else { return; };
            if let Some(frame) = bgra(source, format.size().width, format.size().height, stride, format.format()) {
                on_frame(frame);
            }
        }).register().map_err(|_| "Could not listen to PipeWire stream")?;
    let formats = pod(Value::Object(object!(spa::utils::SpaTypes::ObjectParamFormat, ParamType::EnumFormat,
        property!(FormatProperties::MediaType, Id, MediaType::Video),
        property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        property!(FormatProperties::VideoFormat, Choice, Enum, Id, VideoFormat::BGRx,
            VideoFormat::BGRx, VideoFormat::BGRA, VideoFormat::RGBx, VideoFormat::RGBA),
        property!(FormatProperties::VideoSize, Choice, Range, Rectangle,
            spa::utils::Rectangle { width: 1920, height: 1080 },
            spa::utils::Rectangle { width: 1, height: 1 },
            spa::utils::Rectangle { width: i32::MAX as u32, height: i32::MAX as u32 }),
        property!(FormatProperties::VideoFramerate, Choice, Range, Fraction,
            spa::utils::Fraction { num: 30, denom: 1 },
            spa::utils::Fraction { num: 0, denom: 1 },
            spa::utils::Fraction { num: 30, denom: 1 }),
    )))?;
    let formats = Pod::from_bytes(&formats).ok_or("Invalid PipeWire format parameters")?;
    stream.connect(spa::utils::Direction::Input, Some(node),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut [formats]).map_err(|_| "Could not start PipeWire stream")?;
    mainloop.run();
    let _ = stream.disconnect();
    Ok(())
}

fn bgra(source: &[u8], width: u32, height: u32, stride: i32, format: VideoFormat) -> Option<Frame> {
    if width == 0 || height == 0 { return None; }
    let (swap, opaque) = match format {
        VideoFormat::BGRx => (false, true),
        VideoFormat::BGRA => (false, false),
        VideoFormat::RGBx => (true, true),
        VideoFormat::RGBA => (true, false),
        _ => return None,
    };
    let row = (width as usize).checked_mul(4)?;
    let stride_bytes = stride.unsigned_abs() as usize;
    let height = height as usize;
    if stride_bytes < row || (height - 1).checked_mul(stride_bytes)?.checked_add(row)? > source.len() {
        return None;
    }
    let mut data = vec![0; row.checked_mul(height)?];
    for (y, target) in data.chunks_exact_mut(row).enumerate() {
        let y = if stride < 0 { height - 1 - y } else { y };
        target.copy_from_slice(&source[y * stride_bytes..y * stride_bytes + row]);
        for pixel in target.as_chunks_mut::<4>().0 {
            if swap { pixel.swap(0, 2); }
            if opaque { pixel[3] = 255; }
        }
    }
    Some(Frame { width, height: height as u32, data })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rgbx_padded_stride_to_bgra() {
        let bytes = [1, 2, 3, 0, 4, 5, 6, 0, 99, 99, 99, 99, 7, 8, 9, 0, 10, 11, 12, 0];
        let frame = bgra(&bytes, 2, 2, 12, VideoFormat::RGBx).unwrap();
        assert_eq!((frame.width, frame.height), (2, 2));
        assert_eq!(frame.data, [3, 2, 1, 255, 6, 5, 4, 255, 9, 8, 7, 255, 12, 11, 10, 255]);
        assert!(bgra(&bytes[..19], 2, 2, 12, VideoFormat::RGBx).is_none());
        assert!(bgra(&bytes, 2, 2, 4, VideoFormat::RGBx).is_none());
    }

    #[test]
    fn formats_and_inverted_rows() {
        for (format, expected) in [
            (VideoFormat::BGRx, [1, 2, 3, 255]),
            (VideoFormat::BGRA, [1, 2, 3, 4]),
            (VideoFormat::RGBx, [3, 2, 1, 255]),
            (VideoFormat::RGBA, [3, 2, 1, 4]),
        ] {
            assert_eq!(bgra(&[1, 2, 3, 4], 1, 1, 4, format).unwrap().data, expected);
        }
        assert_eq!(bgra(&[1, 2, 3, 4, 5, 6, 7, 8], 1, 2, -4, VideoFormat::BGRA).unwrap().data,
            [5, 6, 7, 8, 1, 2, 3, 4]);
        assert!(bgra(&[], 0, 0, 0, VideoFormat::Unknown).is_none());
    }
}
