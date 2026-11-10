//! Captures other applications' playback without looping the peer's voice back.
use pipewire::{self as pw, spa};
use spa::param::audio::{AudioFormat, AudioInfoRaw};
use spa::pod::{Pod, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

struct Capture {
    stop: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() { let _ = thread.join(); }
    }
}

pub fn start(on_frame: Box<dyn FnMut(Vec<f32>) + Send>) -> Result<impl Send + 'static, String> {
    let (stop, receiver) = pw::channel::channel();
    let (ready, result) = mpsc::channel();
    let thread = std::thread::Builder::new().name("system-audio".into()).spawn(move || {
        if let Err(error) = run(receiver, ready.clone(), on_frame) { let _ = ready.send(Err(error)); }
    }).map_err(|_| "Could not start system audio thread")?;
    let capture = Capture { stop, thread: Some(thread) };
    result.recv_timeout(Duration::from_secs(10)).map_err(|_| "PipeWire connection timed out")??;
    Ok(capture)
}

struct Port {
    node: u32,
    channel: String,
    input: bool,
}

#[derive(Default)]
struct Graph {
    capture: Option<u32>,
    sources: HashMap<u32, (Option<u32>, Option<u32>)>,
    clients: HashMap<u32, u32>,
    ports: HashMap<u32, Port>,
    links: HashMap<(u32, u32), pw::link::Link>,
}

impl Graph {
    fn external(&self, node: u32) -> bool {
        self.sources.get(&node).and_then(|(client, pid)| pid.or_else(|| client.and_then(|id| self.clients.get(&id).copied())))
            .is_some_and(|pid| pid != std::process::id())
    }

    fn link(&mut self, core: &pw::core::Core) -> Result<(), String> {
        let Some(capture) = self.capture else { return Ok(()); };
        let stale: Vec<_> = self.links.keys().copied().filter(|(output, _)|
            !self.ports.get(output).is_some_and(|port| self.external(port.node))).collect();
        for key in stale { self.links.remove(&key); }
        for (&output_id, output) in &self.ports {
            if output.input || !self.external(output.node) { continue; }
            for (&input_id, input) in &self.ports {
                if !input.input || input.node != capture || !matches!(input.channel.as_str(), "FL" | "FR")
                    || !(output.channel == input.channel || output.channel == "MONO") { continue; }
                if let std::collections::hash_map::Entry::Vacant(entry) = self.links.entry((output_id, input_id)) {
                    let link = core.create_object::<pw::link::Link>("link-factory", &pw::properties::properties! {
                        "link.output.port" => output_id.to_string(),
                        "link.input.port" => input_id.to_string(),
                        "link.output.node" => output.node.to_string(),
                        "link.input.node" => capture.to_string(),
                        "object.linger" => "false",
                    }).map_err(|_| "Could not link system audio")?;
                    entry.insert(link);
                }
            }
        }
        Ok(())
    }

    fn remove(&mut self, id: u32) {
        self.sources.remove(&id);
        self.clients.remove(&id);
        self.sources.retain(|_, (client, _)| *client != Some(id));
        self.ports.retain(|&port_id, port| port_id != id && port.node != id);
        self.links.retain(|&(output, input), _| self.ports.contains_key(&output) && self.ports.contains_key(&input));
    }
}

fn run(stop: pw::channel::Receiver<()>, ready: mpsc::Sender<Result<(), String>>,
    mut on_frame: Box<dyn FnMut(Vec<f32>) + Send>) -> Result<(), String> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|_| "Could not create PipeWire loop")?;
    let _stop = stop.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|_| "Could not create PipeWire context")?;
    let core = context.connect_rc(None).map_err(|_| "Could not connect to PipeWire")?;
    let _core_listener = core.add_listener_local().error({
        let mainloop = mainloop.clone();
        let ready = ready.clone();
        move |id, _, _, _| {
            if id != pw::core::PW_ID_CORE { return; }
            let _ = ready.send(Err("PipeWire connection failed".into()));
            mainloop.quit();
        }
    }).register();
    let graph = Rc::new(RefCell::new(Graph::default()));
    let registry = core.get_registry_rc().map_err(|_| "Could not watch PipeWire graph")?;
    let nodes = Rc::new(RefCell::new(HashMap::new()));
    let clients = Rc::new(RefCell::new(HashMap::new()));
    let _registry_listener = registry.add_listener_local().global({
        let (graph, nodes, clients, registry, core, mainloop, ready) =
            (graph.clone(), nodes.clone(), clients.clone(), registry.clone(), core.clone(), mainloop.clone(), ready.clone());
        move |global| {
            let Some(props) = global.props else { return; };
            if global.type_ == pw::types::ObjectType::Client {
                let Ok(client) = registry.bind::<pw::client::Client, _>(global) else { return; };
                let listener = client.add_listener_local().info({
                    let (graph, core, mainloop, ready) = (graph.clone(), core.clone(), mainloop.clone(), ready.clone());
                    move |info| {
                        let Some(pid) = info.props().and_then(|props| props.get("application.process.id"))
                            .and_then(|pid| pid.parse().ok()) else { return; };
                        let mut graph = graph.borrow_mut();
                        graph.clients.insert(info.id(), pid);
                        if let Err(error) = graph.link(&core) { let _ = ready.send(Err(error)); mainloop.quit(); }
                    }
                }).register();
                clients.borrow_mut().insert(global.id, (listener, client));
            } else if global.type_ == pw::types::ObjectType::Node && props.get("media.class") == Some("Stream/Output/Audio") {
                let client = props.get("client.id").and_then(|id| id.parse().ok());
                graph.borrow_mut().sources.insert(global.id, (client, None));
                let Ok(node) = registry.bind::<pw::node::Node, _>(global) else { return; };
                let listener = node.add_listener_local().info({
                    let (graph, core, mainloop, ready) = (graph.clone(), core.clone(), mainloop.clone(), ready.clone());
                    move |info| {
                        let Some(props) = info.props() else { return; };
                        // Globals omit the PID; subsequent info events only contain changed properties.
                        let Some(pid) = props.get("application.process.id") else { return; };
                        let mut graph = graph.borrow_mut();
                        if let Some((_, node_pid)) = graph.sources.get_mut(&info.id()) {
                            *node_pid = Some(pid.parse().unwrap_or(std::process::id()));
                        }
                        if let Err(error) = graph.link(&core) { let _ = ready.send(Err(error)); mainloop.quit(); }
                    }
                }).register();
                nodes.borrow_mut().insert(global.id, (listener, node));
            } else if global.type_ == pw::types::ObjectType::Port {
                let Some(node) = props.get("node.id").and_then(|id| id.parse().ok()) else { return; };
                let Some(channel) = props.get("audio.channel") else { return; };
                let Some(direction) = props.get("port.direction") else { return; };
                let mut graph = graph.borrow_mut();
                graph.ports.insert(global.id, Port { node, channel: channel.into(), input: direction == "in" });
                if let Err(error) = graph.link(&core) { let _ = ready.send(Err(error)); mainloop.quit(); }
            }
        }
    }).global_remove({
        let (graph, nodes, clients) = (graph.clone(), nodes.clone(), clients.clone());
        move |id| { nodes.borrow_mut().remove(&id); clients.borrow_mut().remove(&id); graph.borrow_mut().remove(id); }
    }).register();
    let stream = pw::stream::StreamBox::new(&core, "crabber-system-audio", pw::properties::properties! {
        "node.name" => "crabber-system-audio",
        "node.description" => "Crabber system audio",
        "media.type" => "Audio",
        "media.category" => "Capture",
        "media.class" => "Stream/Input/Audio",
        "node.autoconnect" => "false",
        "node.dont-reconnect" => "true",
        "adapter.auto-port-config" => "{ mode = dsp position = preserve }",
    }).map_err(|_| "Could not create system audio stream")?;
    let mut format = AudioInfoRaw::new();
    format.set_format(AudioFormat::F32LE);
    format.set_rate(48_000);
    format.set_channels(2);
    let mut position = [0; spa::sys::SPA_AUDIO_MAX_CHANNELS as usize];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    format.set_position(position);
    let _listener = stream.add_local_listener_with_user_data(Vec::with_capacity(1920)).state_changed({
        let (graph, core, mainloop, ready) = (graph.clone(), core.clone(), mainloop.clone(), ready.clone());
        move |stream, _, _, state| match state {
            pw::stream::StreamState::Paused => {
                let mut graph = graph.borrow_mut();
                graph.capture = Some(stream.node_id());
                let result = graph.link(&core);
                if result.is_err() { mainloop.quit(); }
                let _ = ready.send(result);
            }
            pw::stream::StreamState::Error(_) | pw::stream::StreamState::Unconnected => {
                let _ = ready.send(Err("System audio stream failed".into()));
                mainloop.quit();
            }
            _ => {}
        }
    }).process(move |stream, pcm| {
        let Some(mut buffer) = stream.dequeue_buffer() else { return; };
        let Some(data) = buffer.datas_mut().first_mut() else { return; };
        if data.as_raw().chunk.is_null() { return; }
        let chunk = data.chunk();
        if chunk.size() == 0 || chunk.flags().bits() & 3 != 0 { return; }
        let (offset, size) = (chunk.offset() as usize, chunk.size() as usize);
        let Some(bytes) = data.data().filter(|bytes| !bytes.is_empty()) else { return; };
        let offset = offset % bytes.len();
        let Some(bytes) = offset.checked_add(size).and_then(|end| bytes.get(offset..end)) else { return; };
        for stereo in bytes.as_chunks::<8>().0 {
            let (left, right) = stereo.split_at(4);
            let left = f32::from_le_bytes([left[0], left[1], left[2], left[3]]);
            let right = f32::from_le_bytes([right[0], right[1], right[2], right[3]]);
            pcm.push((left + right) * 0.5);
            if pcm.len() == 960 { on_frame(std::mem::replace(pcm, Vec::with_capacity(1920))); }
        }
    }).register().map_err(|_| "Could not listen to system audio stream")?;
    let format = Value::Object(spa::pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: format.into(),
    });
    let (format, _) = spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &format)
        .map_err(|_| "Could not configure system audio")?;
    let format = format.into_inner();
    let format = Pod::from_bytes(&format).ok_or("Invalid system audio format")?;
    stream.connect(spa::utils::Direction::Input, None, pw::stream::StreamFlags::MAP_BUFFERS, &mut [format])
        .map_err(|_| "Could not start system audio stream")?;
    mainloop.run();
    graph.borrow_mut().links.clear();
    let _ = stream.disconnect();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::io::Write;
    use std::process::{Command, Stdio};
    use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
    use std::time::Instant;

    #[test]
    fn process_identity_fails_closed_and_node_pid_wins() {
        let mut graph = Graph::default();
        let ours = std::process::id();
        let other = ours + 1;
        graph.sources.insert(10, (Some(1), None));
        assert!(!graph.external(10));
        graph.clients.insert(1, ours);
        assert!(!graph.external(10));
        graph.clients.insert(1, other);
        assert!(graph.external(10));
        graph.sources.insert(10, (Some(1), Some(ours)));
        assert!(!graph.external(10));
        graph.sources.insert(11, (None, Some(other)));
        assert!(graph.external(11));
        graph.remove(1);
        assert!(!graph.external(10));
    }

    fn measure(frames: &mpsc::Receiver<Vec<f32>>, duration: Duration) -> (usize, f32) {
        let end = Instant::now() + duration;
        let (mut samples, mut energy) = (0, 0.0f64);
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            if let Ok(frame) = frames.recv_timeout(left) {
                assert_eq!(frame.len(), 960);
                samples += frame.len();
                energy += frame.iter().map(|&s| f64::from(s).powi(2)).sum::<f64>();
            } else { break; }
        }
        (samples, (energy / samples.max(1) as f64).sqrt() as f32)
    }

    #[test]
    #[ignore = "plays audible tones; requires PipeWire, pw-cat, and otherwise silent applications"]
    fn captures_other_process_but_not_ours() {
        let (send, frames) = mpsc::channel();
        let host = cpal::host_from_id(cpal::HostId::PipeWire).unwrap();
        let device = host.output_devices().unwrap()
            .find(|device| device.description().is_ok_and(|info| info.driver().is_some())).unwrap();
        let target = device.id().unwrap();
        let supported = device.default_output_config().unwrap();
        assert_eq!(supported.sample_format(), cpal::SampleFormat::F32);
        let config = supported.config();
        let (channels, rate) = (config.channels as usize, config.sample_rate as f32);
        let played = Arc::new(AtomicUsize::new(0));
        let count = played.clone();
        let mut n = 0u64;
        let own = device.build_output_stream(config, move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
            for frame in data.chunks_mut(channels) {
                frame.fill((n as f32 * std::f32::consts::TAU * 660.0 / rate).sin() * 0.15);
                n += 1;
            }
            count.fetch_add(data.len() / channels, Ordering::Relaxed);
        }, |error| eprintln!("test playback: {error}"), None).unwrap();
        own.play().unwrap();
        let capture = start(Box::new(move |frame| { let _ = send.send(frame); })).unwrap();
        let _ = measure(&frames, Duration::from_millis(500));
        let ours = measure(&frames, Duration::from_secs(2));
        println!("own process: {} samples, RMS {:.8}, {} output samples played", ours.0, ours.1, played.load(Ordering::Relaxed));
        assert!(played.load(Ordering::Relaxed) >= 48_000, "own output stream did not play");
        assert!(ours.1 < 0.0001, "own playback leaked (or another application is playing)");
        drop(own);

        // Start another application after capture, exercising registry hotplug too.
        for channels in [2, 1] {
            let mut child = Command::new("pw-cat").args(["--playback", "--raw", "--format", "f32",
                "--rate", "48000", "--channels", &channels.to_string(), "--target", target.id(), "-"])
                .stdin(Stdio::piped()).spawn().unwrap();
            let mut stdin = child.stdin.take().unwrap();
            let writer = std::thread::spawn(move || {
                let mut tone = Vec::new();
                for n in 0..144_000 {
                    let sample = (n as f32 * std::f32::consts::TAU * 440.0 / 48_000.0).sin() * 0.15;
                    for _ in 0..channels { tone.extend_from_slice(&sample.to_le_bytes()); }
                }
                stdin.write_all(&tone).unwrap();
            });
            let _ = measure(&frames, Duration::from_millis(500));
            let external = measure(&frames, Duration::from_secs(2));
            writer.join().unwrap();
            assert!(child.wait().unwrap().success());
            println!("external {channels}ch: {} samples, RMS {:.8}", external.0, external.1);
            assert!(external.0 >= 48_000 && external.1 > 0.01, "external playback was not captured");
            let _ = measure(&frames, Duration::from_millis(500));
        }
        drop(capture);
        let nodes = Command::new("pw-cli").args(["ls", "Node"]).output().unwrap();
        let links = Command::new("pw-link").arg("-l").output().unwrap();
        assert!(nodes.status.success() && links.status.success());
        assert!(!String::from_utf8_lossy(&nodes.stdout).contains("crabber-system-audio"));
        assert!(!String::from_utf8_lossy(&links.stdout).contains("crabber-system-audio"));
    }
}
