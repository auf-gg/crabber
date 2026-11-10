//! Audio and video calls: XEP-0353 (Jingle Message Initiation) for ringing, XEP-0166/0167/0176/0320
//! (Jingle RTP over ICE-UDP with DTLS-SRTP) for the session, webrtc-rs for the media stack.
//! Signalling is translated between Jingle XML and the SDP that webrtc-rs speaks.
//! Video is VP8 in a BUNDLE with the audio; an audio call gains video through content-add.
use crate::t;
use std::collections::{BTreeMap, VecDeque};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::{FutureExt as _, StreamExt};
use futures::channel::mpsc::{UnboundedSender, unbounded};
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_OPUS, MIME_TYPE_PCMU, MIME_TYPE_VP8, MediaEngine};
use rtc::rtp::packetizer::Depacketizer;
use rtc::rtp_transceiver::rtp_sender::{RTCPFeedback, RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind};
use tokio_xmpp::IqRequest;
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::minidom::rxml::Namespace;
use tokio_xmpp::parsers::message::{Message, MessageType};
use tokio_xmpp::parsers::stanza_error::DefinedCondition;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceCandidateInit, RTCIceServer,
    RTCPeerConnectionIceEvent, RTCPeerConnectionState, RTCSessionDescription, Registry, register_default_interceptors,
};
use webrtc::rtp_transceiver::RtpSender;

use crate::audio::{self, Audio};
use crate::videocall::{Decoder, Encoder, FRAMES, PREVIEW, Source, is_keyframe, keyframe_size};
use crate::xmpp::{Ev, Net, nc};

const NS_JINGLE: &str = "urn:xmpp:jingle:1";
const NS_RTP: &str = "urn:xmpp:jingle:apps:rtp:1";
const NS_ICE: &str = "urn:xmpp:jingle:transports:ice-udp:1";
const NS_DTLS: &str = "urn:xmpp:jingle:apps:dtls:0";
const NS_JMI: &str = "urn:xmpp:jingle-message:0";
const NS_FB: &str = "urn:xmpp:jingle:apps:rtp:rtcp-fb:0";
const NS_HDREXT: &str = "urn:xmpp:jingle:apps:rtp:rtp-hdrext:0";
const NS_SSMA: &str = "urn:xmpp:jingle:apps:rtp:ssma:0";
const NS_GROUP: &str = "urn:xmpp:jingle:apps:grouping:0";
const NS_HINTS: &str = "urn:xmpp:hints";
const NS_RTP_INFO: &str = "urn:xmpp:jingle:apps:rtp:info:1";
const NS_JINGLE_ERRORS: &str = "urn:xmpp:jingle:errors:1";

const ACTIONS: [&str; 15] = [
    "content-accept",
    "content-add",
    "content-modify",
    "content-reject",
    "content-remove",
    "description-info",
    "security-info",
    "session-accept",
    "session-info",
    "session-initiate",
    "session-terminate",
    "transport-accept",
    "transport-info",
    "transport-reject",
    "transport-replace",
];

/// Our payload types in offers; answers echo the offerer's (see `update_formats`).
const VP8_PT: u8 = 96;
const OPUS_PT: u8 = 111;

/// Whether a screen share sends system audio; read when a call starts (later: `CallCmd::ShareAudio`).
static SHARE_AUDIO: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_share_audio(on: bool) {
    SHARE_AUDIO.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Received audio RTP packets across all calls; lets tests and diagnostics see that media flows.
pub static RTP_RECEIVED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub enum CallCmd {
    Start { to: String, video: bool },
    /// `video`: answer with the camera on.
    Accept { video: bool },
    Hangup,
    Mute(bool),
    /// Stop playing the peer (our sound off).
    Deafen(bool),
    /// What we send as video; turning it on in an audio call adds video to the session.
    Video(Option<Source>),
    /// The chosen microphone, speaker or camera changed: reopen what the call is using.
    Devices,
    /// The screen share quality changed: restart its encoder.
    ScreenQuality,
    /// Send what other applications play along with a screen share.
    ShareAudio(bool),
}

#[derive(Clone, Debug, PartialEq)]
pub enum CallState {
    Outgoing,
    Incoming,
    Connecting,
    Active,
    Ended(String),
}

#[derive(Debug)]
pub enum CallEv {
    State { peer: String, state: CallState },
    /// The call carries video, and what we send right now.
    Media { video: bool, sending: Option<Source> },
    /// Something went wrong without ending the call (camera, screen picker, video declined).
    Notice(String),
    /// What the peer told us (XEP-0167 session-info, XEP-0353 ringing).
    Peer { muted: bool, held: bool, ringing: bool },
}

/// Why a Jingle request is refused: the stanza error, with the Jingle condition when there is one (XEP-0166 §10).
#[derive(Debug, PartialEq)]
pub struct JingleError {
    pub condition: DefinedCondition,
    pub jingle: Option<&'static str>,
}

impl JingleError {
    fn new(condition: DefinedCondition, jingle: Option<&'static str>) -> Self {
        Self { condition, jingle }
    }

    /// `<error>` content: the Jingle condition in its own namespace.
    pub fn jingle_element(&self) -> Option<Element> {
        self.jingle.map(|j| Element::builder(j, NS_JINGLE_ERRORS).build())
    }

    /// `modify` for requests the sender could fix, `cancel` otherwise.
    pub fn retry(&self) -> bool {
        matches!(self.condition, DefinedCondition::BadRequest | DefinedCondition::FeatureNotImplemented)
    }
}

/// What incoming Jingle requests are checked against: they are answered in the XMPP loop, before
/// the actor sees them.
#[derive(Default)]
struct Session {
    sid: Option<String>,
    initiator: bool,
    /// Our session-initiate waits for session-accept.
    awaiting_accept: bool,
    /// Our content-add waits for content-accept / content-reject.
    adding: bool,
}

/// Whether we take a Jingle request (XEP-0166 §10).
fn check(el: &Element, s: &Session) -> Result<(), JingleError> {
    let action = el.attr("action").unwrap_or_default();
    let refuse = |c, j| Err(JingleError::new(c, j));
    if !ACTIONS.contains(&action) {
        return refuse(DefinedCondition::BadRequest, None);
    }
    if action != "session-initiate" && s.sid.as_deref() != el.attr("sid") {
        return refuse(DefinedCondition::ItemNotFound, Some("unknown-session"));
    }
    match action {
        "session-accept" if !s.awaiting_accept => refuse(DefinedCondition::UnexpectedRequest, Some("out-of-order")),
        "content-accept" | "content-reject" if !s.adding => refuse(DefinedCondition::UnexpectedRequest, Some("out-of-order")),
        // Both added media at once: the initiator's addition wins (§7.2.16).
        "content-add" if s.adding && s.initiator => refuse(DefinedCondition::Conflict, Some("tie-break")),
        "session-info" if !known_info(el) => refuse(DefinedCondition::FeatureNotImplemented, Some("unsupported-info")),
        _ => Ok(()),
    }
}

/// session-info payloads we understand; an empty session-info is a ping (XEP-0166 §6.8).
fn known_info(el: &Element) -> bool {
    el.children().all(|c| c.ns() == NS_RTP_INFO && matches!(c.name(), "active" | "hold" | "unhold" | "mute" | "unmute" | "ringing"))
}

enum Msg {
    Cmd(CallCmd),
    Jingle(Jid, Element),
    Jmi(Jid, Element),
    Candidate(String, RTCIceCandidateInit),
    PcState(String, RTCPeerConnectionState),
    Track(String, Arc<dyn TrackRemote>),
    Timeout(String),
    Encoder(String, Source, Result<Encoder, String>),
}

/// Front end owned by the XMPP loop; the call itself runs in its own task.
pub struct Calls {
    tx: UnboundedSender<Msg>,
    me: BareJid,
    session: Arc<Mutex<Session>>,
    bound: Arc<Mutex<Option<Jid>>>,
}

impl Calls {
    pub fn new(net: Net, me: BareJid) -> Self {
        let (tx, mut rx) = unbounded();
        let session = Arc::new(Mutex::new(Session::default()));
        let bound = Arc::new(Mutex::new(None));
        let mut actor = Actor { net, me: me.clone(), tx: tx.clone(), call: None, session: session.clone(), bound: bound.clone() };
        tokio::spawn(async move {
            while let Some(msg) = rx.next().await {
                // A bug in one call must not take every later call with it: drop that call, go on.
                if std::panic::AssertUnwindSafe(actor.handle(msg)).catch_unwind().await.is_err() {
                    actor.end(t!("call.reason_dropped")).await;
                }
            }
        });
        Self { tx, me, session, bound }
    }

    pub fn set_bound(&self, jid: Jid) {
        *self.bound.lock().unwrap() = Some(jid);
    }

    pub fn on_cmd(&mut self, cmd: CallCmd) {
        let _ = self.tx.unbounded_send(Msg::Cmd(cmd));
    }

    /// Validates synchronously so the IQ can be answered; the work happens in the actor.
    pub fn on_jingle(&self, from: Option<Jid>, el: Element) -> Result<(), JingleError> {
        let from = from.ok_or(JingleError::new(DefinedCondition::BadRequest, None))?;
        check(&el, &self.session.lock().unwrap())?;
        let _ = self.tx.unbounded_send(Msg::Jingle(from, el));
        Ok(())
    }

    /// `archived`: from MAM/offline storage — a stale ring that must not pop up.
    pub fn on_jmi(&mut self, from: &Jid, _to: Option<&Jid>, el: Element, archived: bool) {
        if archived && el.name() == "propose" {
            return;
        }
        let _ = self.tx.unbounded_send(Msg::Jmi(from.clone(), el));
        let _ = &self.me;
    }
}

/// Our video track once added: track, SSRC and the negotiated VP8 payload type.
type VideoTrack = Arc<Mutex<Option<(Arc<TrackLocalStaticSample>, u32, u8)>>>;

/// What our audio goes out as: Opus unless the peer only has PCMU, under the negotiated payload type.
#[derive(Clone, Copy, Debug, PartialEq)]
struct AudioFormat {
    opus: bool,
    pt: u8,
}

type AudioOut = Arc<Mutex<AudioFormat>>;

struct Call {
    sid: String,
    peer_bare: BareJid,
    /// Full JID once known (proceed / session-initiate).
    peer: Option<Jid>,
    initiator: bool,
    state: CallState,
    pc: Option<Arc<dyn PeerConnection>>,
    /// Incoming session-initiate received before the user answered.
    pending_offer: Option<Element>,
    pending_candidates: Vec<RTCIceCandidateInit>,
    remote_set: bool,
    /// Content names (SDP mids) in m-line order.
    contents: Vec<String>,
    /// The peer's contents as last described, in m-line order: its SDP when renegotiating.
    remote: Vec<Element>,
    /// Our content-add waiting for content-accept / content-reject.
    adding: Vec<String>,
    ufrag: String,
    audio: Option<Arc<Audio>>,
    audio_out: AudioOut,
    /// Ring / ringback while the call is not answered yet.
    tone: Option<audio::Tone>,
    video: bool,
    video_track: VideoTrack,
    video_sender: Option<Arc<dyn RtpSender>>,
    /// Encoded frames on their way to the video track (dropped while there is none).
    frames: UnboundedSender<Vec<u8>>,
    encoder: Option<Encoder>,
    sending: Option<Source>,
    muted: bool,
    deafened: bool,
    /// What the peer reported: mic off, holding us, ringing.
    peer_muted: bool,
    peer_held: bool,
    peer_ringing: bool,
    /// Whether to send system audio with a screen share, its capture while running, and the
    /// captured samples waiting to be mixed into ours.
    share_audio: bool,
    system_audio: Option<crate::sysaudio::SystemAudio>,
    system_buf: Arc<Mutex<VecDeque<f32>>>,
    /// Keeps the computer awake while the call is up.
    awake: Option<crate::awake::Awake>,
}

struct Actor {
    net: Net,
    me: BareJid,
    tx: UnboundedSender<Msg>,
    call: Option<Call>,
    session: Arc<Mutex<Session>>,
    bound: Arc<Mutex<Option<Jid>>>,
}

fn jmi(name: &str, sid: &str) -> Element {
    Element::builder(name, NS_JMI).attr(nc("id"), sid).build()
}

fn chat_with(to: Jid, payload: Element) -> Message {
    let hint = Element::builder("store", NS_HINTS).build();
    let mut m = Message::new_with_type(MessageType::Chat, Some(to)).with_payloads(vec![payload, hint]);
    m.id = Some(tokio_xmpp::parsers::message::Id(crate::xmpp::new_id()));
    m
}

fn media_of(content: &Element) -> Option<&str> {
    content.get_child("description", NS_RTP).and_then(|d| d.attr("media")).filter(|m| *m == "audio" || *m == "video")
}

/// The audio/video contents of a jingle element.
fn rtp_contents(j: &Element) -> Vec<Element> {
    j.children().filter(|c| c.is("content", NS_JINGLE) && media_of(c).is_some()).cloned().collect()
}

fn has_video(contents: &[Element]) -> bool {
    contents.iter().any(|c| media_of(c) == Some("video"))
}

/// Writes encoded frames to the video track, timed by their arrival.
fn spawn_video_writer(track: VideoTrack) -> UnboundedSender<Vec<u8>> {
    let (tx, mut rx) = unbounded::<Vec<u8>>();
    tokio::spawn(async move {
        let mut prev: Option<Instant> = None;
        while let Some(frame) = rx.next().await {
            let now = Instant::now();
            let duration = prev.map_or(Duration::from_millis(33), |p| (now - p).clamp(Duration::from_millis(1), Duration::from_secs(1)));
            prev = Some(now);
            let Some((track, ssrc, pt)) = track.lock().unwrap().clone() else { continue };
            let sample = Sample {
                data: bytes::Bytes::from(frame),
                timestamp: now,
                duration,
                packet_timestamp: 0,
                prev_dropped_packets: 0,
                prev_padding_packets: 0,
            };
            let _ = track.write_sample(ssrc, pt, &sample, &[]).await;
        }
    });
    tx
}

/// Encodes 20 ms frames in the negotiated codec and sends them as RTP: the codec is known only
/// after negotiation, so packets are built here rather than by a codec-bound track.
async fn write_audio(mut frames: futures::channel::mpsc::UnboundedReceiver<Vec<f32>>, track: Arc<TrackLocalStaticRTP>, ssrc: u32, out: AudioOut, system: Arc<Mutex<VecDeque<f32>>>) {
    let (mut seq, mut ts): (u16, u32) = (rand::random(), rand::random());
    let mut opus = None;
    while let Some(mut pcm) = frames.next().await {
        // System audio of a screen share rides along with the microphone.
        {
            let mut sys = system.lock().unwrap();
            for x in pcm.iter_mut() {
                let Some(s) = sys.pop_front() else { break };
                *x = (*x + s).clamp(-1.0, 1.0);
            }
        }
        let f = *out.lock().unwrap();
        let payload = if f.opus {
            if opus.is_none() {
                opus = audio::OpusEncoder::new().ok();
            }
            match opus.as_mut().and_then(|e| e.encode(&pcm)) {
                Some(p) => p,
                None => continue,
            }
        } else {
            audio::ulaw_frame(&pcm)
        };
        let header = rtc::rtp::header::Header { version: 2, payload_type: f.pt, sequence_number: seq, timestamp: ts, ssrc, ..Default::default() };
        seq = seq.wrapping_add(1);
        ts = ts.wrapping_add(if f.opus { audio::CALL_FRAME as u32 } else { audio::FRAME as u32 });
        // Errors before the transport is up are expected; frames are just dropped.
        let _ = track.write_rtp(rtc::rtp::Packet { header, payload: bytes::Bytes::from(payload) }).await;
    }
}

/// Reassembles VP8 frames from RTP and decodes them for the UI; asks for a keyframe after loss.
/// The decoder runs at the picture's own size, restarted when a keyframe brings a new one.
async fn receive_video(track: Arc<dyn TrackRemote>) {
    let mut dec: Option<(Decoder, (u32, u32))> = None;
    let mut depack = rtc::rtp::codec::vp8::Vp8Packet::default();
    let (mut frame, mut last_seq, mut broken, mut last_pli) = (Vec::new(), None::<u16>, true, None::<Instant>);
    while let Some(ev) = track.poll().await {
        let TrackRemoteEvent::OnRtpPacket(p) = ev else { continue };
        if last_seq.is_some_and(|s| p.header.sequence_number != s.wrapping_add(1)) {
            frame.clear();
            broken = true;
        }
        last_seq = Some(p.header.sequence_number);
        match depack.depacketize(&p.payload) {
            // A frame starts at partition 0 with the start bit; anything else mid-frame after loss is skipped.
            Ok(b) if !frame.is_empty() || (depack.s == 1 && depack.pid == 0) => frame.extend_from_slice(&b),
            Ok(_) => continue,
            Err(_) => {
                frame.clear();
                broken = true;
                continue;
            }
        }
        if !p.header.marker {
            continue;
        }
        let f = std::mem::take(&mut frame);
        if broken && !is_keyframe(&f) {
            if last_pli.is_none_or(|t| t.elapsed() > Duration::from_millis(500)) {
                last_pli = Some(Instant::now());
                let pli = rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication { sender_ssrc: 0, media_ssrc: p.header.ssrc };
                let _ = track.write_rtcp(vec![Box::new(pli)]).await;
            }
            continue;
        }
        if let Some(size) = keyframe_size(&f)
            && dec.as_ref().is_none_or(|d| d.1 != size)
        {
            dec = Decoder::start(size, &FRAMES.remote).ok().map(|d| (d, size));
        }
        broken = !dec.as_ref().is_some_and(|(d, _)| d.push(f));
    }
}

impl Actor {
    fn emit(&self, state: CallState) {
        if let Some(c) = &self.call {
            self.net.emit(Ev::Call(CallEv::State { peer: c.peer_bare.to_string(), state }));
        }
    }

    fn emit_media(&self) {
        if let Some(c) = &self.call {
            self.net.emit(Ev::Call(CallEv::Media { video: c.video, sending: c.sending }));
        }
    }

    fn emit_peer(&self) {
        if let Some(c) = &self.call {
            self.net.emit(Ev::Call(CallEv::Peer { muted: c.peer_muted, held: c.peer_held, ringing: c.peer_ringing }));
        }
    }

    fn notice(&self, text: String) {
        self.net.emit(Ev::Call(CallEv::Notice(text)));
    }

    fn set_state(&mut self, state: CallState) {
        if let Some(c) = self.call.as_mut() {
            c.state = state.clone();
            if !matches!(state, CallState::Outgoing | CallState::Incoming) {
                c.tone = None;
            }
        }
        self.emit(state);
    }

    /// Tears the call down locally; signalling is up to the caller.
    async fn end(&mut self, reason: &str) {
        self.emit(CallState::Ended(reason.to_string()));
        if let Some(c) = self.call.take()
            && let Some(pc) = c.pc
        {
            let _ = pc.close().await;
        }
        FRAMES.clear();
        *self.session.lock().unwrap() = Session::default();
    }

    async fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Cmd(cmd) => self.on_cmd(cmd).await,
            Msg::Jmi(from, el) => self.on_jmi(from, el).await,
            Msg::Jingle(from, el) => self.on_jingle(from, el).await,
            Msg::Candidate(sid, cand) => {
                let Some(c) = self.call.as_ref().filter(|c| c.sid == sid) else { return };
                let Some(peer) = c.peer.clone() else { return };
                if let Some(cand_el) = candidate_to_jingle(&cand.candidate) {
                    let transport = Element::builder("transport", NS_ICE).attr(nc("ufrag"), c.ufrag.as_str()).append(cand_el);
                    let el = self.jingle("transport-info", &sid).append(self.content(c, transport)).build();
                    self.iq(peer, el);
                }
            }
            Msg::PcState(sid, state) => {
                if self.call.as_ref().is_none_or(|c| c.sid != sid) {
                    return;
                }
                match state {
                    RTCPeerConnectionState::Connected => {
                        if let Some(c) = self.call.as_mut()
                            && c.awake.is_none()
                        {
                            c.awake = crate::awake::keep_awake(t!("call.awake_reason"));
                        }
                        self.set_state(CallState::Active)
                    }
                    RTCPeerConnectionState::Failed => {
                        self.terminate("failed-transport").await;
                        self.end(t!("call.reason_connect_failed")).await;
                    }
                    _ => {}
                }
            }
            Msg::Track(sid, track) => {
                let Some(c) = self.call.as_ref().filter(|c| c.sid == sid) else { return };
                let audio = c.audio.clone();
                tokio::spawn(async move {
                    if track.kind().await == RtpCodecKind::Video {
                        return receive_video(track).await;
                    }
                    let Some(audio) = audio else { return };
                    // Only Opus and PCMU (static type 0) are negotiated.
                    let mut opus = None;
                    while let Some(ev) = track.poll().await {
                        let TrackRemoteEvent::OnRtpPacket(p) = ev else { continue };
                        RTP_RECEIVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        let (pcm, rate) = if p.header.payload_type == 0 {
                            (audio::ulaw_samples(&p.payload), audio::RATE)
                        } else {
                            if opus.is_none() {
                                opus = audio::OpusDecoder::new().ok();
                            }
                            let Some(d) = opus.as_mut() else { continue };
                            (d.decode(&p.payload), audio::CALL_RATE)
                        };
                        audio::note_level(true, &pcm);
                        audio.play(&pcm, rate);
                    }
                });
            }
            Msg::Timeout(sid) => {
                if let Some(c) = self.call.as_ref().filter(|c| c.sid == sid && c.state == CallState::Outgoing) {
                    self.net.send(chat_with(c.peer_bare.clone().into(), jmi("retract", &sid)));
                    self.end(t!("call.reason_no_answer")).await;
                }
            }
            Msg::Encoder(sid, src, res) => {
                // Stale: another call, or the source was switched/turned off while the picker was open.
                let Some(c) = self.call.as_mut().filter(|c| c.sid == sid && c.sending == Some(src)) else { return };
                match res {
                    Ok(e) => c.encoder = Some(e),
                    Err(e) => {
                        c.sending = None;
                        FRAMES.local.lock().unwrap().frame = None;
                        self.notice(t!("call.video_failed", error = e).to_string());
                        self.emit_media();
                    }
                }
            }
        }
    }

    async fn on_cmd(&mut self, cmd: CallCmd) {
        match cmd {
            CallCmd::Start { to, video } => {
                if self.call.is_some() {
                    return;
                }
                let Ok(peer_bare) = BareJid::from_str(&to) else { return };
                let sid = crate::xmpp::new_id();
                let mut propose = Element::builder("propose", NS_JMI)
                    .attr(nc("id"), sid.as_str())
                    .append(Element::builder("description", NS_RTP).attr(nc("media"), "audio"));
                if video {
                    propose = propose.append(Element::builder("description", NS_RTP).attr(nc("media"), "video"));
                }
                self.net.send(chat_with(peer_bare.clone().into(), propose.build()));
                self.start_call(sid.clone(), peer_bare, None, true, CallState::Outgoing, video);
                if video {
                    self.set_video(Some(Source::Camera)).await;
                }
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    let _ = tx.unbounded_send(Msg::Timeout(sid));
                });
            }
            CallCmd::Accept { video } => {
                let Some(c) = self.call.as_ref().filter(|c| c.state == CallState::Incoming) else { return };
                let (sid, offer) = (c.sid.clone(), c.pending_offer.clone());
                if let Some(peer) = c.peer.clone() {
                    self.net.send(chat_with(peer, jmi("proceed", &sid)));
                }
                // Tell our other devices to stop ringing.
                self.net.send(chat_with(self.me.clone().into(), jmi("accept", &sid)));
                self.set_state(CallState::Connecting);
                if let Some(offer) = offer {
                    self.answer(offer).await;
                }
                if video && self.call.as_ref().is_some_and(|c| c.video) {
                    self.set_video(Some(Source::Camera)).await;
                }
            }
            CallCmd::Hangup => {
                let Some(c) = self.call.as_ref() else { return };
                let (sid, bare, peer) = (c.sid.clone(), c.peer_bare.clone(), c.peer.clone());
                match (&c.state, c.pc.is_some() || c.pending_offer.is_some()) {
                    (CallState::Outgoing, false) => self.net.send(chat_with(bare.into(), jmi("retract", &sid))),
                    (CallState::Incoming, false) => {
                        if let Some(peer) = peer {
                            self.net.send(chat_with(peer, jmi("reject", &sid)));
                        }
                        self.net.send(chat_with(self.me.clone().into(), jmi("reject", &sid)));
                    }
                    (CallState::Incoming, true) => self.terminate("decline").await,
                    // The initiator gives up before the call is up.
                    (CallState::Outgoing | CallState::Connecting, true) if c.initiator => self.terminate("cancel").await,
                    _ => self.terminate("success").await,
                }
                self.end("").await;
            }
            // Kept on the call too: the audio may not exist yet (still ringing).
            CallCmd::Mute(m) => {
                let Some(c) = self.call.as_mut() else { return };
                c.muted = m;
                self.apply_mute();
                self.mute_info("audio", m);
            }
            CallCmd::Deafen(d) => {
                let Some(c) = self.call.as_mut() else { return };
                c.deafened = d;
                if let Some(a) = c.audio.as_ref() {
                    a.deafened.store(d, std::sync::atomic::Ordering::Relaxed);
                }
            }
            CallCmd::Video(src) => self.set_video(src).await,
            CallCmd::Devices => {
                let Some(c) = self.call.as_ref() else { return };
                let (audio, camera) = (c.audio.clone(), c.sending == Some(Source::Camera));
                if let Some(a) = audio {
                    // A device can take a moment to open; keep the actor free meanwhile.
                    if let Ok(Err(e)) = tokio::task::spawn_blocking(move || a.restart()).await {
                        self.notice(t!("call.reason_error", error = e).to_string());
                    }
                }
                if camera {
                    self.set_video(Some(Source::Camera)).await;
                }
            }
            CallCmd::ScreenQuality => {
                let Some(c) = self.call.as_ref().filter(|c| c.sending == Some(Source::Screen)) else { return };
                // Wayland keeps its portal session; elsewhere the capture restarts on the same target.
                if !c.encoder.as_ref().is_some_and(|e| e.restart()) {
                    self.set_video(Some(Source::Screen)).await;
                }
            }
            CallCmd::ShareAudio(on) => {
                if let Some(c) = self.call.as_mut() {
                    c.share_audio = on;
                }
                self.update_system_audio();
            }
        }
    }

    /// System audio runs only while the screen is shared and the user asked for it.
    fn update_system_audio(&mut self) {
        let Some(c) = self.call.as_mut() else { return };
        let want = c.share_audio && c.sending == Some(Source::Screen);
        if !want {
            c.system_audio = None;
            c.system_buf.lock().unwrap().clear();
            return;
        }
        if c.system_audio.is_some() {
            return;
        }
        let buf = c.system_buf.clone();
        let res = crate::sysaudio::start(move |frame| {
            let mut b = buf.lock().unwrap();
            b.extend(frame);
            // Its clock drifts from the microphone's: keep at most 200 ms.
            let max = audio::CALL_RATE as usize / 5;
            if b.len() > max {
                let excess = b.len() - max;
                b.drain(..excess);
            }
        });
        match res {
            Ok(capture) => c.system_audio = Some(capture),
            Err(e) => self.notice(t!("call.system_audio_failed", error = e).to_string()),
        }
    }

    /// Switches what we send; the first video in an audio call is negotiated with content-add.
    async fn set_video(&mut self, src: Option<Source>) {
        let Some(c) = self.call.as_mut() else { return };
        // Stop the old capture first: the camera can't be opened twice.
        c.encoder = None;
        let was = std::mem::replace(&mut c.sending, src);
        FRAMES.local.lock().unwrap().frame = None;
        if was.is_some() != src.is_some() {
            self.mute_info("video", src.is_none());
        }
        self.update_system_audio();
        let Some(src) = src else { return self.emit_media() };
        let Some(c) = self.call.as_mut() else { return };
        let upgrade = c.pc.is_some() && c.video_track.lock().unwrap().is_none();
        if c.pc.is_none() && !c.video {
            // Ringing audio call: the proposal can't change any more.
            c.sending = None;
            return self.emit_media();
        }
        if upgrade && let Err(e) = self.add_video_content().await {
            if let Some(c) = self.call.as_mut() {
                c.sending = None;
            }
            self.notice(t!("call.video_failed", error = e).to_string());
            return self.emit_media();
        }
        self.emit_media();
        let Some(c) = self.call.as_ref() else { return };
        let (sid, frames, tx) = (c.sid.clone(), c.frames.clone(), self.tx.clone());
        tokio::task::spawn_blocking(move || {
            let preview = Decoder::start(PREVIEW, &FRAMES.local).ok();
            let res = Encoder::start(src, move |f| {
                if let Some(p) = &preview {
                    p.push(f.clone());
                }
                let _ = frames.unbounded_send(f);
            });
            let _ = tx.unbounded_send(Msg::Encoder(sid, src, res));
        });
    }

    fn start_call(&mut self, sid: String, peer_bare: BareJid, peer: Option<Jid>, initiator: bool, state: CallState, video: bool) {
        *self.session.lock().unwrap() = Session { sid: Some(sid.clone()), initiator, ..Default::default() };
        FRAMES.clear();
        let video_track = VideoTrack::default();
        self.call = Some(Call {
            sid,
            peer_bare,
            peer,
            initiator,
            state: state.clone(),
            pc: None,
            pending_offer: None,
            pending_candidates: vec![],
            remote_set: false,
            contents: vec![],
            remote: vec![],
            adding: vec![],
            ufrag: String::new(),
            audio: None,
            audio_out: Arc::new(Mutex::new(AudioFormat { opus: true, pt: OPUS_PT })),
            tone: Some(audio::tone(state == CallState::Incoming)),
            video,
            frames: spawn_video_writer(video_track.clone()),
            video_track,
            video_sender: None,
            encoder: None,
            sending: None,
            muted: false,
            deafened: false,
            peer_muted: false,
            peer_held: false,
            peer_ringing: false,
            share_audio: SHARE_AUDIO.load(std::sync::atomic::Ordering::Relaxed),
            system_audio: None,
            system_buf: Arc::default(),
            awake: None,
        });
        self.emit(state);
        self.emit_media();
    }

    async fn on_jmi(&mut self, from: Jid, el: Element) {
        let Some(sid) = el.attr("id").map(str::to_string) else { return };
        let from_self = from.to_bare() == self.me;
        match el.name() {
            "propose" if !from_self => {
                let media = |m: &str| el.children().any(|d| d.is("description", NS_RTP) && d.attr("media") == Some(m));
                if !media("audio") {
                    return;
                }
                let video = media("video");
                if let Some(c) = self.call.as_ref() {
                    if c.sid == sid {
                        return;
                    }
                    // Both called each other at once: the lower session id wins (XEP-0166 §7.2.16).
                    // Ours lost: withdraw it and take theirs as answered; theirs lost: they yield to ours.
                    let crossed = c.initiator && c.state == CallState::Outgoing && c.pc.is_none() && c.peer_bare == from.to_bare();
                    if !crossed {
                        self.net.send(chat_with(from, jmi("reject", &sid)));
                    } else if sid.as_bytes() < c.sid.as_bytes() {
                        let (ours, wanted_video) = (c.sid.clone(), c.video);
                        self.net.send(chat_with(c.peer_bare.clone().into(), jmi("retract", &ours)));
                        self.call = None;
                        self.start_call(sid, from.to_bare(), Some(from), false, CallState::Incoming, video);
                        self.on_cmd(CallCmd::Accept { video: wanted_video }).await;
                    }
                    return;
                }
                // Tell the caller we are ringing (XEP-0353 §3.2).
                self.net.send(chat_with(from.clone(), Element::builder("ringing", NS_JMI).attr(nc("id"), sid.as_str()).build()));
                self.start_call(sid, from.to_bare(), Some(from), false, CallState::Incoming, video);
            }
            "ringing" if !from_self => {
                let Some(c) = self.call.as_mut().filter(|c| c.sid == sid && c.state == CallState::Outgoing) else { return };
                c.peer_ringing = true;
                self.emit_peer();
            }
            "retract" if self.call.as_ref().is_some_and(|c| c.sid == sid) => self.end(t!("call.reason_retracted")).await,
            "reject" if self.call.as_ref().is_some_and(|c| c.sid == sid) => {
                let reason = if from_self { t!("call.reason_rejected_elsewhere") } else { t!("call.reason_declined") };
                self.end(reason).await;
            }
            "accept" if from_self && self.call.as_ref().is_some_and(|c| c.sid == sid && c.state == CallState::Incoming) => {
                self.end(t!("call.reason_accepted_elsewhere")).await;
            }
            "proceed" if !from_self => {
                let Some(c) = self.call.as_mut().filter(|c| c.sid == sid && c.state == CallState::Outgoing) else { return };
                c.peer = Some(from);
                self.set_state(CallState::Connecting);
                self.offer().await;
            }
            "finish" if self.call.as_ref().is_some_and(|c| c.sid == sid) => self.end("").await,
            _ => {}
        }
    }

    async fn on_jingle(&mut self, from: Jid, el: Element) {
        let sid = el.attr("sid").unwrap_or_default().to_string();
        match el.attr("action").unwrap_or_default() {
            "session-initiate" => {
                let same = self.call.as_ref().is_some_and(|c| c.sid == sid);
                if self.call.is_some() && !same {
                    let busy = self.jingle("session-terminate", &sid).append(reason("busy")).build();
                    self.iq(from, busy);
                    return;
                }
                if !same {
                    // Jingle without a JMI ring first: ring now.
                    let video = has_video(&rtp_contents(&el));
                    self.start_call(sid.clone(), from.to_bare(), Some(from.clone()), false, CallState::Incoming, video);
                }
                let c = self.call.as_mut().unwrap();
                c.peer = Some(from.clone());
                if c.state == CallState::Connecting {
                    self.answer(el).await;
                } else {
                    c.pending_offer = Some(el);
                    if !same {
                        let ringing = Element::builder("ringing", NS_RTP_INFO).build();
                        self.iq(from, self.jingle("session-info", &sid).append(ringing).build());
                    }
                }
            }
            "session-accept" => {
                self.session.lock().unwrap().awaiting_accept = false;
                let Some(c) = self.call.as_mut() else { return };
                let Some(pc) = c.pc.clone() else { return };
                c.remote = rtp_contents(&el);
                let sdp = contents_to_sdp(&c.remote, peer_role(c));
                let ok = match RTCSessionDescription::answer(sdp.clone()) {
                    Ok(desc) => pc.set_remote_description(desc).await.is_ok(),
                    Err(_) => false,
                };
                match ok {
                    true => {
                        self.update_formats(&sdp);
                        self.flush_candidates(&pc).await;
                        self.announce_mute();
                    }
                    false => {
                        self.terminate("failed-application").await;
                        self.end(t!("call.reason_incompatible")).await;
                    }
                }
            }
            "content-add" => self.on_content_add(from, &el).await,
            "content-accept" => {
                self.session.lock().unwrap().adding = false;
                let Some(c) = self.call.as_mut() else { return };
                let Some(pc) = c.pc.clone() else { return };
                let adding = std::mem::take(&mut c.adding);
                let accepted = rtp_contents(&el).into_iter().filter(|e| e.attr("name").is_some_and(|n| adding.iter().any(|a| a == n)));
                c.remote.extend(accepted);
                let sdp = contents_to_sdp(&c.remote, peer_role(c));
                let ok = match RTCSessionDescription::answer(sdp) {
                    Ok(desc) => pc.set_remote_description(desc).await.is_ok(),
                    Err(_) => false,
                };
                if !ok {
                    self.notice(t!("call.video_failed", error = t!("call.reason_incompatible")).to_string());
                }
            }
            "content-reject" | "content-remove" => {
                let Some(c) = self.call.as_mut() else { return };
                let names: Vec<String> = el.children().filter(|e| e.is("content", NS_JINGLE)).filter_map(|e| e.attr("name")).map(str::to_string).collect();
                if !names.iter().any(|n| c.adding.contains(n)) {
                    if el.attr("action") == Some("content-remove") {
                        self.remove_contents(&names).await;
                    }
                    return;
                }
                self.session.lock().unwrap().adding = false;
                let Some(c) = self.call.as_mut() else { return };
                // Our video offer was declined: undo it so a later attempt starts clean.
                let adding = std::mem::take(&mut c.adding);
                c.contents.retain(|n| !adding.contains(n));
                c.video = false;
                c.sending = None;
                c.encoder = None;
                *c.video_track.lock().unwrap() = None;
                let (pc, sender) = (c.pc.clone(), c.video_sender.take());
                if let Some(pc) = pc {
                    if let Ok(rb) = RTCSessionDescription::rollback(None) {
                        let _ = pc.set_local_description(rb).await;
                    }
                    if let Some(s) = sender {
                        let _ = pc.remove_track(&s).await;
                    }
                }
                FRAMES.local.lock().unwrap().frame = None;
                self.notice(t!("call.video_declined").to_string());
                self.emit_media();
            }
            "session-info" => self.on_session_info(&el),
            // Who sends changes: noted for later offers; our video stops if we may no longer send it.
            "content-modify" => {
                let Some(c) = self.call.as_mut() else { return };
                let me = if c.initiator { "initiator" } else { "responder" };
                let mut stop_video = false;
                for m in el.children().filter(|e| e.is("content", NS_JINGLE)) {
                    if let (Some(name), Some(senders)) = (m.attr("name"), m.attr("senders"))
                        && let Some(r) = c.remote.iter_mut().find(|r| r.attr("name") == Some(name))
                    {
                        r.set_attr(Namespace::NONE, nc("senders"), senders);
                        stop_video |= media_of(r) == Some("video") && senders != "both" && senders != me;
                    }
                }
                if stop_video && c.sending.is_some() {
                    self.set_video(None).await;
                }
            }
            // ICE-UDP is the only transport we have (XEP-0166 §7.2.15).
            "transport-replace" => {
                let mut reject = self.jingle("transport-reject", &sid);
                for content in el.children().filter(|e| e.is("content", NS_JINGLE)) {
                    reject = reject.append(content.clone());
                }
                self.iq(from, reject.build());
            }
            "transport-info" => {
                let Some(c) = self.call.as_mut() else { return };
                let mut cands = vec![];
                for content in el.children().filter(|e| e.is("content", NS_JINGLE)) {
                    let mid = content.attr("name").unwrap_or("0").to_string();
                    for t in content.children().filter(|e| e.is("transport", NS_ICE)) {
                        for cand in t.children().filter(|e| e.is("candidate", NS_ICE)) {
                            if let Some(s) = jingle_to_candidate(cand) {
                                cands.push(RTCIceCandidateInit {
                                    candidate: s,
                                    sdp_mid: Some(mid.clone()),
                                    sdp_mline_index: Some(0),
                                    username_fragment: t.attr("ufrag").map(str::to_string),
                                    ..Default::default()
                                });
                            }
                        }
                    }
                }
                c.pending_candidates.extend(cands);
                if c.remote_set
                    && let Some(pc) = c.pc.clone()
                {
                    self.flush_candidates(&pc).await;
                }
            }
            "session-terminate" => {
                let why = el
                    .get_child("reason", NS_JINGLE)
                    .and_then(|r| r.children().next())
                    .map(|c| c.name().to_string())
                    .unwrap_or_default();
                let text = match why.as_str() {
                    "success" | "" => "",
                    "busy" => t!("call.reason_busy"),
                    "decline" => t!("call.reason_declined"),
                    "cancel" => t!("call.reason_retracted"),
                    "timeout" | "expired" => t!("call.reason_no_answer"),
                    "gone" => t!("call.reason_gone"),
                    "connectivity-error" | "failed-transport" => t!("call.reason_connect_failed"),
                    "incompatible-parameters" | "unsupported-applications" | "unsupported-transports" | "failed-application" => t!("call.reason_incompatible"),
                    "security-error" => t!("call.reason_security"),
                    "media-error" => t!("call.reason_media"),
                    _ => t!("call.reason_dropped"),
                };
                self.end(text).await;
            }
            _ => {}
        }
    }

    /// The peer adds media (normally video to an audio call): accepted right away, our camera stays off.
    async fn on_content_add(&mut self, from: Jid, el: &Element) {
        let Some(c) = self.call.as_mut() else { return };
        let Some(pc) = c.pc.clone() else { return };
        // Both added media at once and the initiator's wins (XEP-0166 §7.2.16): withdraw ours, keeping
        // its track, which then answers their content.
        if !c.initiator && !c.adding.is_empty() {
            self.session.lock().unwrap().adding = false;
            let adding = std::mem::take(&mut c.adding);
            c.contents.retain(|n| !adding.contains(n));
            if let Ok(rb) = RTCSessionDescription::rollback(None) {
                let _ = pc.set_local_description(rb).await;
            }
        }
        let added: Vec<Element> = rtp_contents(el).into_iter().filter(|e| e.attr("name").is_some_and(|n| !c.contents.iter().any(|x| x == n))).collect();
        if added.is_empty() {
            return;
        }
        let names: Vec<String> = added.iter().filter_map(|e| e.attr("name")).map(str::to_string).collect();
        let mut remote = c.remote.clone();
        remote.extend(added.iter().cloned());
        let sdp = contents_to_sdp(&remote, peer_role(c));
        let need_track = has_video(&added) && c.video_track.lock().unwrap().is_none();
        let res: Result<String, String> = async {
            let desc = RTCSessionDescription::offer(sdp).map_err(|e| e.to_string())?;
            pc.set_remote_description(desc).await.map_err(|e| e.to_string())?;
            if need_track {
                self.add_video_track(&pc).await?;
            }
            let answer = pc.create_answer(None).await.map_err(|e| e.to_string())?;
            pc.set_local_description(answer).await.map_err(|e| e.to_string())?;
            Ok(pc.local_description().await.ok_or(t!("call.err_no_sdp").to_string())?.sdp)
        }
        .await;
        match res {
            Ok(sdp) => {
                let Some(c) = self.call.as_mut() else { return };
                c.remote = remote;
                c.contents.extend(names.iter().cloned());
                c.video |= has_video(&added);
                self.update_formats(&sdp);
                self.send_contents("content-accept", &sdp, Some(&names));
                self.emit_media();
            }
            Err(_) => {
                if let Ok(rb) = RTCSessionDescription::rollback(None) {
                    let _ = pc.set_remote_description(rb).await;
                }
                let mut rej = self.jingle("content-reject", el.attr("sid").unwrap_or_default());
                for e in &added {
                    rej = rej.append(Element::builder("content", NS_JINGLE).attr(nc("creator"), e.attr("creator").unwrap_or("initiator")).attr(nc("name"), e.attr("name").unwrap_or_default()));
                }
                self.iq(from, rej.build());
            }
        }
    }

    /// The peer removed established contents: their media stops (the m-lines stay, inactive); with
    /// nothing left the session ends (XEP-0166 §7.2.8).
    async fn remove_contents(&mut self, names: &[String]) {
        let Some(c) = self.call.as_mut() else { return };
        for r in c.remote.iter_mut().filter(|r| r.attr("name").is_some_and(|n| names.iter().any(|x| x == n))) {
            r.set_attr(Namespace::NONE, nc("senders"), "none");
        }
        let live = |m: &str| c.remote.iter().any(|r| media_of(r) == Some(m) && r.attr("senders") != Some("none"));
        if !live("audio") && !live("video") {
            self.terminate("success").await;
            return self.end("").await;
        }
        if !live("video") && c.video {
            c.video = false;
            *FRAMES.remote.lock().unwrap() = Default::default();
            self.set_video(None).await;
        }
    }

    async fn flush_candidates(&mut self, pc: &Arc<dyn PeerConnection>) {
        let Some(c) = self.call.as_mut() else { return };
        c.remote_set = true;
        for cand in std::mem::take(&mut c.pending_candidates) {
            let _ = pc.add_ice_candidate(cand).await;
        }
    }

    fn jingle(&self, action: &str, sid: &str) -> tokio_xmpp::minidom::ElementBuilder {
        let me = self.bound.lock().unwrap().clone().map(|j| j.to_string()).unwrap_or_else(|| self.me.to_string());
        let role = if action == "session-initiate" { "initiator" } else { "responder" };
        let b = Element::builder("jingle", NS_JINGLE).attr(nc("action"), action).attr(nc("sid"), sid);
        if action == "session-initiate" || action == "session-accept" { b.attr(nc(role), me) } else { b }
    }

    /// Candidates go to the first content: everything is bundled on one transport.
    fn content(&self, c: &Call, child: tokio_xmpp::minidom::ElementBuilder) -> tokio_xmpp::minidom::ElementBuilder {
        let name = c.contents.first().map_or("0", String::as_str);
        Element::builder("content", NS_JINGLE).attr(nc("creator"), "initiator").attr(nc("name"), name).append(child)
    }

    fn iq(&self, to: Jid, el: Element) {
        let net = self.net.clone();
        tokio::spawn(async move {
            let _ = net.iq(Some(to), IqRequest::Set(el)).await;
        });
    }

    /// XEP-0167 session-info to the peer, once the session exists.
    fn session_info(&self, payload: Element) {
        let Some(c) = self.call.as_ref().filter(|c| c.pc.is_some()) else { return };
        let Some(peer) = c.peer.clone() else { return };
        let el = self.jingle("session-info", &c.sid).append(payload).build();
        self.iq(peer, el);
    }

    /// Tells the peer we muted or unmuted our `media` content, if the session has it.
    fn mute_info(&self, media: &str, mute: bool) {
        let Some(content) = self.call.as_ref().and_then(|c| c.remote.iter().find(|e| media_of(e) == Some(media))) else { return };
        let el = Element::builder(if mute { "mute" } else { "unmute" }, NS_RTP_INFO)
            .attr(nc("creator"), content.attr("creator").unwrap_or("initiator"))
            .attr(nc("name"), content.attr("name").unwrap_or_default())
            .build();
        self.session_info(el);
    }

    /// Our audio goes silent when we muted it, and while the peer holds the call (XEP-0167 §7.2).
    fn apply_mute(&self) {
        if let Some(c) = &self.call
            && let Some(a) = &c.audio
        {
            a.muted.store(c.muted || c.peer_held, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// A mute made while still ringing reaches the peer once the session is up.
    fn announce_mute(&self) {
        if self.call.as_ref().is_some_and(|c| c.muted) {
            self.mute_info("audio", true);
        }
    }

    /// The peer's session-info: ringing, hold, mute (XEP-0167 §7). An empty one is a ping.
    fn on_session_info(&mut self, el: &Element) {
        let Some(c) = self.call.as_mut() else { return };
        for info in el.children().filter(|e| e.ns() == NS_RTP_INFO) {
            match info.name() {
                "ringing" => c.peer_ringing = true,
                "hold" => c.peer_held = true,
                "unhold" => c.peer_held = false,
                "active" => (c.peer_held, c.peer_muted) = (false, false),
                "mute" | "unmute" => {
                    let on = info.name() == "mute";
                    // No name: every content of the session.
                    let media = info.attr("name").and_then(|n| c.remote.iter().find(|e| e.attr("name") == Some(n))).and_then(media_of);
                    if media != Some("video") {
                        c.peer_muted = on;
                    }
                    if media != Some("audio") && on {
                        // Their camera is off: drop the last picture now rather than when frames stop.
                        *FRAMES.remote.lock().unwrap() = Default::default();
                    }
                }
                _ => {}
            }
        }
        self.apply_mute();
        self.emit_peer();
    }

    async fn terminate(&mut self, why: &str) {
        let Some(c) = self.call.as_ref() else { return };
        let Some(peer) = c.peer.clone() else { return };
        let el = self.jingle("session-terminate", &c.sid).append(reason(why)).build();
        self.iq(peer, el);
    }

    /// Builds the peer connection, local audio track (and video track for a video call) and audio devices.
    async fn setup_pc(&mut self) -> Result<Arc<dyn PeerConnection>, String> {
        let c = self.call.as_ref().ok_or(t!("call.err_no_call"))?;
        let (sid, video) = (c.sid.clone(), c.video);
        let ice_servers = ice_servers(&self.net, self.me.domain().as_str()).await;

        let mut media = media_engine()?;
        let registry = register_default_interceptors(Registry::new(), &mut media).map_err(|e| e.to_string())?;
        let handler = Arc::new(Handler { sid: sid.clone(), tx: self.tx.clone() });
        let pc = PeerConnectionBuilder::new()
            .with_configuration(RTCConfigurationBuilder::new().with_ice_servers(ice_servers).build())
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_handler(handler)
            .with_udp_addrs(vec![format!("{}:0", local_ip())])
            .build()
            .await
            .map_err(|e| e.to_string())?;
        let pc: Arc<dyn PeerConnection> = Arc::new(pc);

        let ssrc = std::hash::BuildHasher::hash_one(&std::collections::hash_map::RandomState::new(), &sid) as u32;
        let track = Arc::new(audio_track(&sid, ssrc));
        pc.add_track(track.clone() as Arc<dyn TrackLocal>).await.map_err(|e| e.to_string())?;
        if video {
            self.add_video_track(&pc).await?;
        }

        let (frames_tx, frames_rx) = unbounded::<Vec<f32>>();
        let audio = audio::start(move |frame| {
            let _ = frames_tx.unbounded_send(frame);
        })?;
        let c = self.call.as_ref().ok_or(t!("call.err_no_call"))?;
        tokio::spawn(write_audio(frames_rx, track, ssrc, c.audio_out.clone(), c.system_buf.clone()));
        let c = self.call.as_mut().ok_or(t!("call.err_no_call"))?;
        audio.muted.store(c.muted || c.peer_held, std::sync::atomic::Ordering::Relaxed);
        audio.deafened.store(c.deafened, std::sync::atomic::Ordering::Relaxed);
        c.audio = Some(Arc::new(audio));
        c.pc = Some(pc.clone());
        Ok(pc)
    }

    /// Adds our VP8 track; frames start flowing into it from the writer task.
    async fn add_video_track(&mut self, pc: &Arc<dyn PeerConnection>) -> Result<(), String> {
        let c = self.call.as_ref().ok_or(t!("call.err_no_call"))?;
        let sid = c.sid.clone();
        let ssrc = std::hash::BuildHasher::hash_one(&std::collections::hash_map::RandomState::new(), format!("{sid}-video")) as u32;
        let track = Arc::new(
            TrackLocalStaticSample::new(
                Instant::now(),
                MediaStreamTrack::new(
                    "crabber".into(),
                    format!("video-{sid}"),
                    "video".into(),
                    RtpCodecKind::Video,
                    vec![RTCRtpEncodingParameters {
                        rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(ssrc), ..Default::default() },
                        codec: vp8(),
                        ..Default::default()
                    }],
                ),
            )
            .map_err(|e| e.to_string())?,
        );
        let sender = pc.add_track(track.clone() as Arc<dyn TrackLocal>).await.map_err(|e| e.to_string())?;
        let c = self.call.as_mut().ok_or(t!("call.err_no_call"))?;
        *c.video_track.lock().unwrap() = Some((track, ssrc, VP8_PT));
        c.video_sender = Some(sender);
        Ok(())
    }

    /// Picks what we send from a negotiated SDP (our answer, or the peer's answer to our offer):
    /// answers echo the offerer's payload types.
    fn update_formats(&self, sdp: &str) {
        let Some(c) = self.call.as_ref() else { return };
        let (audio, vp8) = formats(sdp);
        if let Some(a) = audio {
            *c.audio_out.lock().unwrap() = a;
        }
        if let (Some(pt), Some(t)) = (vp8, c.video_track.lock().unwrap().as_mut()) {
            t.2 = pt;
        }
    }

    /// Video in an established audio call: re-offer and send only the new m-line as content-add.
    async fn add_video_content(&mut self) -> Result<(), String> {
        let pc = self.call.as_ref().and_then(|c| c.pc.clone()).ok_or(t!("call.err_no_call"))?;
        self.add_video_track(&pc).await?;
        let offer = pc.create_offer(None).await.map_err(|e| e.to_string())?;
        pc.set_local_description(offer).await.map_err(|e| e.to_string())?;
        let sdp = pc.local_description().await.ok_or(t!("call.err_no_sdp").to_string())?.sdp;
        let c = self.call.as_mut().ok_or(t!("call.err_no_call"))?;
        let new: Vec<String> = parse_sdp(&sdp).sections.into_iter().map(|s| s.mid).filter(|m| !c.contents.contains(m)).collect();
        c.adding = new.clone();
        c.contents.extend(new.iter().cloned());
        c.video = true;
        self.send_contents("content-add", &sdp, Some(&new));
        Ok(())
    }

    async fn offer(&mut self) {
        let pc = match self.setup_pc().await {
            Ok(pc) => pc,
            Err(e) => return self.fail(&e).await,
        };
        let res = async {
            let offer = pc.create_offer(None).await.map_err(|e| e.to_string())?;
            pc.set_local_description(offer).await.map_err(|e| e.to_string())?;
            pc.local_description().await.ok_or(t!("call.err_no_sdp").to_string())
        }
        .await;
        match res {
            Ok(local) => self.send_contents("session-initiate", &local.sdp, None),
            Err(e) => self.fail(&e).await,
        }
    }

    async fn answer(&mut self, offer: Element) {
        if let Some(c) = self.call.as_mut() {
            c.remote = rtp_contents(&offer);
            c.video = has_video(&c.remote);
        }
        let pc = match self.setup_pc().await {
            Ok(pc) => pc,
            Err(e) => return self.fail(&e).await,
        };
        let sdp = self.call.as_ref().map(|c| contents_to_sdp(&c.remote, peer_role(c))).unwrap_or_default();
        let res = async {
            let desc = RTCSessionDescription::offer(sdp).map_err(|e| e.to_string())?;
            pc.set_remote_description(desc).await.map_err(|e| e.to_string())?;
            let answer = pc.create_answer(None).await.map_err(|e| e.to_string())?;
            pc.set_local_description(answer).await.map_err(|e| e.to_string())?;
            pc.local_description().await.ok_or(t!("call.err_no_sdp").to_string())
        }
        .await;
        match res {
            Ok(local) => {
                self.flush_candidates(&pc).await;
                self.update_formats(&local.sdp);
                self.send_contents("session-accept", &local.sdp, None);
                self.announce_mute();
                self.emit_media();
            }
            Err(e) => self.fail(&e).await,
        }
    }

    /// Sends the m-lines of `sdp` as Jingle contents: all of them, or only `only` (content-add/accept).
    fn send_contents(&mut self, action: &str, sdp: &str, only: Option<&[String]>) {
        let Some(c) = self.call.as_mut() else { return };
        let Some(peer) = c.peer.clone() else { return };
        // Set before sending: the reply is checked against it as soon as it arrives.
        let mut session = self.session.lock().unwrap();
        session.awaiting_accept |= action == "session-initiate";
        session.adding |= action == "content-add";
        drop(session);
        let parsed = parse_sdp(sdp);
        if only.is_none() {
            c.contents = parsed.sections.iter().map(|s| s.mid.clone()).collect();
        }
        c.ufrag = parsed.ufrag.clone();
        let (sid, initiator) = (c.sid.clone(), c.initiator);
        let all = c.contents.clone();
        let mut el = self.jingle(action, &sid);
        for (i, s) in parsed.sections.iter().enumerate() {
            if only.is_none_or(|o| o.contains(&s.mid)) {
                el = el.append(sdp_to_content(&parsed, s, initiator, i == 0));
            }
        }
        if action != "content-accept" {
            let mut group = Element::builder("group", NS_GROUP).attr(nc("semantics"), "BUNDLE");
            for name in &all {
                group = group.append(Element::builder("content", NS_GROUP).attr(nc("name"), name.as_str()));
            }
            el = el.append(group);
        }
        self.iq(peer, el.build());
    }

    async fn fail(&mut self, error: &str) {
        self.terminate("failed-application").await;
        self.end(&t!("call.reason_error", error = error)).await;
    }
}

/// Opus first (preferred), PCMU as the fallback every WebRTC stack has, VP8 for video.
fn media_engine() -> Result<MediaEngine, String> {
    let mut media = MediaEngine::default();
    let codecs = [(opus(), OPUS_PT, RtpCodecKind::Audio), (pcmu(), 0, RtpCodecKind::Audio), (vp8(), VP8_PT, RtpCodecKind::Video)];
    for (rtp_codec, payload_type, kind) in codecs {
        media.register_codec(RTCRtpCodecParameters { rtp_codec, payload_type }, kind).map_err(|e| e.to_string())?;
    }
    Ok(media)
}

fn opus() -> RTCRtpCodec {
    RTCRtpCodec { mime_type: MIME_TYPE_OPUS.into(), clock_rate: 48000, channels: 2, sdp_fmtp_line: "minptime=10;useinbandfec=1".into(), rtcp_feedback: vec![] }
}

fn pcmu() -> RTCRtpCodec {
    RTCRtpCodec { mime_type: MIME_TYPE_PCMU.into(), clock_rate: 8000, channels: 1, sdp_fmtp_line: String::new(), rtcp_feedback: vec![] }
}

/// The audio track carries packets we build (`write_audio`); with no codec of its own it offers
/// every registered audio codec.
fn audio_track(sid: &str, ssrc: u32) -> TrackLocalStaticRTP {
    let enc = RTCRtpEncodingParameters { rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(ssrc), ..Default::default() }, ..Default::default() };
    TrackLocalStaticRTP::new(MediaStreamTrack::new("crabber".into(), format!("audio-{sid}"), "audio".into(), RtpCodecKind::Audio, vec![enc]))
}

/// The audio format and VP8 payload type a negotiated SDP settles on.
fn formats(sdp: &str) -> (Option<AudioFormat>, Option<u8>) {
    let p = parse_sdp(sdp);
    let find = |media: &str, name: &str| {
        p.sections.iter().filter(|s| s.media == media).flat_map(|s| s.payloads.iter()).find(|x| x.1.eq_ignore_ascii_case(name)).and_then(|x| x.0.parse::<u8>().ok())
    };
    let audio = find("audio", "opus").map(|pt| AudioFormat { opus: true, pt }).or_else(|| find("audio", "PCMU").map(|pt| AudioFormat { opus: false, pt }));
    (audio, find("video", "VP8"))
}

fn vp8() -> RTCRtpCodec {
    let fb = |t: &str, p: &str| RTCPFeedback { typ: t.into(), parameter: p.into() };
    RTCRtpCodec {
        mime_type: MIME_TYPE_VP8.into(),
        clock_rate: 90000,
        channels: 0,
        sdp_fmtp_line: String::new(),
        rtcp_feedback: vec![fb("nack", ""), fb("nack", "pli"), fb("ccm", "fir")],
    }
}

/// The peer's Jingle role in this session.
fn peer_role(c: &Call) -> &'static str {
    if c.initiator { "responder" } else { "initiator" }
}

struct Handler {
    sid: String,
    tx: UnboundedSender<Msg>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
        if let Ok(init) = event.candidate.to_json() {
            let _ = self.tx.unbounded_send(Msg::Candidate(self.sid.clone(), init));
        }
    }
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let _ = self.tx.unbounded_send(Msg::PcState(self.sid.clone(), state));
    }
    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let _ = self.tx.unbounded_send(Msg::Track(self.sid.clone(), track));
    }
}

fn reason(why: &str) -> Element {
    Element::builder("reason", NS_JINGLE).append(Element::builder(why, NS_JINGLE)).build()
}

/// The LAN address to gather host candidates on (no packet is sent by `connect`).
fn local_ip() -> std::net::IpAddr {
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("8.8.8.8:80").map(|_| s))
        .and_then(|s| s.local_addr())
        .map(|a| a.ip())
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))
}

/// STUN/TURN servers announced by the XMPP server (XEP-0215), e.g. ejabberd's mod_stun_disco.
async fn ice_servers(net: &Net, domain: &str) -> Vec<RTCIceServer> {
    let Ok(server) = Jid::from_str(domain) else { return vec![] };
    let query = Element::builder("services", "urn:xmpp:extdisco:2").build();
    let Ok(Some(el)) = net.iq(Some(server), IqRequest::Get(query)).await else { return vec![] };
    el.children()
        .filter(|s| s.name() == "service")
        .filter_map(|s| {
            let (kind, host, port) = (s.attr("type")?, s.attr("host")?, s.attr("port")?);
            let url = match kind {
                "stun" => format!("stun:{host}:{port}"),
                "turn" => format!("turn:{host}:{port}?transport={}", s.attr("transport").unwrap_or("udp")),
                _ => return None,
            };
            Some(RTCIceServer {
                urls: vec![url],
                username: s.attr("username").unwrap_or_default().into(),
                credential: s.attr("password").unwrap_or_default().into(),
            })
        })
        .collect()
}

#[derive(Default, Debug)]
struct Section {
    media: String,
    mid: String,
    dir: String,
    /// id -> (name, clockrate, channels), in m-line order
    payloads: Vec<(String, String, String, String)>,
    order: Vec<String>,
    fmtp: BTreeMap<String, Vec<(String, String)>>,
    rtcp_fb: BTreeMap<String, Vec<(String, Option<String>)>>,
    extmap: Vec<(String, String)>,
    ssrc: BTreeMap<String, Vec<(String, String)>>,
    rtcp_mux: bool,
}

/// ICE and DTLS are shared by all m-lines (BUNDLE), so they are kept once.
#[derive(Default, Debug)]
struct ParsedSdp {
    ufrag: String,
    pwd: String,
    fingerprint: Option<(String, String)>,
    setup: String,
    sections: Vec<Section>,
    candidates: Vec<String>,
}

fn parse_sdp(sdp: &str) -> ParsedSdp {
    let mut p = ParsedSdp::default();
    for line in sdp.lines().map(str::trim) {
        if let Some(m) = line.strip_prefix("m=") {
            let mut it = m.split_whitespace();
            let media = it.next().unwrap_or_default().to_string();
            let order = it.skip(2).map(str::to_string).collect();
            let mid = p.sections.len().to_string();
            p.sections.push(Section { media, mid, dir: "sendrecv".into(), order, ..Default::default() });
        } else if let Some(a) = line.strip_prefix("a=") {
            let (key, val) = a.split_once(':').unwrap_or((a, ""));
            match key {
                "ice-ufrag" => p.ufrag = val.to_string(),
                "ice-pwd" => p.pwd = val.to_string(),
                "setup" => p.setup = val.to_string(),
                "fingerprint" => {
                    if let Some((h, f)) = val.split_once(' ') {
                        p.fingerprint = Some((h.to_string(), f.to_string()));
                    }
                }
                "candidate" => p.candidates.push(format!("candidate:{val}")),
                _ => {
                    let Some(s) = p.sections.last_mut() else { continue };
                    match key {
                        "mid" => s.mid = val.to_string(),
                        "sendrecv" | "sendonly" | "recvonly" | "inactive" => s.dir = key.to_string(),
                        "rtcp-mux" => s.rtcp_mux = true,
                        "rtpmap" => {
                            if let Some((id, rest)) = val.split_once(' ') {
                                let mut parts = rest.split('/');
                                let name = parts.next().unwrap_or_default().to_string();
                                let rate = parts.next().unwrap_or_default().to_string();
                                let ch = parts.next().unwrap_or("1").to_string();
                                s.payloads.push((id.to_string(), name, rate, ch));
                            }
                        }
                        "fmtp" => {
                            if let Some((id, rest)) = val.split_once(' ') {
                                let params = rest.split(';').filter_map(|kv| kv.trim().split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();
                                s.fmtp.insert(id.to_string(), params);
                            }
                        }
                        "rtcp-fb" => {
                            let mut it = val.split_whitespace();
                            if let (Some(id), Some(t)) = (it.next(), it.next()) {
                                s.rtcp_fb.entry(id.to_string()).or_default().push((t.to_string(), it.next().map(str::to_string)));
                            }
                        }
                        "extmap" => {
                            let mut it = val.split_whitespace();
                            if let (Some(id), Some(uri)) = (it.next(), it.next()) {
                                s.extmap.push((id.split('/').next().unwrap_or(id).to_string(), uri.to_string()));
                            }
                        }
                        "ssrc" => {
                            if let Some((id, rest)) = val.split_once(' ') {
                                let (k, v) = rest.split_once(':').unwrap_or((rest, ""));
                                s.ssrc.entry(id.to_string()).or_default().push((k.to_string(), v.to_string()));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    for s in &mut p.sections {
        let order = std::mem::take(&mut s.order);
        s.payloads.sort_by_key(|(id, ..)| order.iter().position(|o| o == id).unwrap_or(usize::MAX));
    }
    p
}

/// One m-line as a Jingle content; `candidates` only on the first (bundled) one.
fn sdp_to_content(p: &ParsedSdp, s: &Section, initiator: bool, candidates: bool) -> Element {
    let (me, peer) = if initiator { ("initiator", "responder") } else { ("responder", "initiator") };
    let senders = match s.dir.as_str() {
        "sendonly" => me,
        "recvonly" => peer,
        "inactive" => "none",
        _ => "both",
    };
    let mut desc = Element::builder("description", NS_RTP).attr(nc("media"), s.media.as_str());
    for (id, pname, rate, ch) in &s.payloads {
        let mut pt = Element::builder("payload-type", NS_RTP).attr(nc("id"), id.as_str()).attr(nc("name"), pname.as_str()).attr(nc("clockrate"), rate.as_str());
        if s.media == "audio" {
            pt = pt.attr(nc("channels"), ch.as_str());
        }
        for (k, v) in s.fmtp.get(id).into_iter().flatten() {
            pt = pt.append(Element::builder("parameter", NS_RTP).attr(nc("name"), k.as_str()).attr(nc("value"), v.as_str()));
        }
        for (t, sub) in s.rtcp_fb.get(id).into_iter().flatten() {
            let mut fb = Element::builder("rtcp-fb", NS_FB).attr(nc("type"), t.as_str());
            if let Some(sub) = sub {
                fb = fb.attr(nc("subtype"), sub.as_str());
            }
            pt = pt.append(fb);
        }
        desc = desc.append(pt);
    }
    for (id, uri) in &s.extmap {
        desc = desc.append(Element::builder("rtp-hdrext", NS_HDREXT).attr(nc("id"), id.as_str()).attr(nc("uri"), uri.as_str()));
    }
    for (ssrc, params) in &s.ssrc {
        let mut src = Element::builder("source", NS_SSMA).attr(nc("ssrc"), ssrc.as_str());
        for (k, v) in params {
            src = src.append(Element::builder("parameter", NS_SSMA).attr(nc("name"), k.as_str()).attr(nc("value"), v.as_str()));
        }
        desc = desc.append(src);
    }
    if s.rtcp_mux {
        desc = desc.append(Element::builder("rtcp-mux", NS_RTP));
    }
    let mut transport = Element::builder("transport", NS_ICE).attr(nc("ufrag"), p.ufrag.as_str()).attr(nc("pwd"), p.pwd.as_str());
    if let Some((hash, fp)) = &p.fingerprint {
        transport = transport.append(Element::builder("fingerprint", NS_DTLS).attr(nc("hash"), hash.as_str()).attr(nc("setup"), p.setup.as_str()).append(fp.as_str()));
    }
    for c in p.candidates.iter().filter(|_| candidates) {
        if let Some(el) = candidate_to_jingle(c) {
            transport = transport.append(el);
        }
    }
    Element::builder("content", NS_JINGLE)
        .attr(nc("creator"), "initiator")
        .attr(nc("name"), s.mid.as_str())
        .attr(nc("senders"), senders)
        .append(desc)
        .append(transport)
        .build()
}

/// "candidate:foundation component proto priority ip port typ type [raddr x rport y] ..."
fn candidate_to_jingle(c: &str) -> Option<Element> {
    let body = c.strip_prefix("candidate:")?;
    let f: Vec<&str> = body.split_whitespace().collect();
    if f.len() < 8 || f[6] != "typ" {
        return None;
    }
    let mut el = Element::builder("candidate", NS_ICE)
        .attr(nc("foundation"), f[0])
        .attr(nc("component"), f[1])
        .attr(nc("protocol"), f[2].to_lowercase())
        .attr(nc("priority"), f[3])
        .attr(nc("ip"), f[4])
        .attr(nc("port"), f[5])
        .attr(nc("type"), f[7])
        .attr(nc("generation"), "0")
        .attr(nc("id"), crate::xmpp::new_id());
    let mut i = 8;
    while i + 1 < f.len() {
        match f[i] {
            "raddr" => el = el.attr(nc("rel-addr"), f[i + 1]),
            "rport" => el = el.attr(nc("rel-port"), f[i + 1]),
            "generation" => el = el.attr(nc("generation"), f[i + 1]),
            _ => {}
        }
        i += 2;
    }
    Some(el.build())
}

fn jingle_to_candidate(c: &Element) -> Option<String> {
    let mut s = format!(
        "candidate:{} {} {} {} {} {} typ {}",
        c.attr("foundation")?,
        c.attr("component")?,
        c.attr("protocol")?,
        c.attr("priority")?,
        c.attr("ip")?,
        c.attr("port")?,
        c.attr("type")?
    );
    if let (Some(a), Some(p)) = (c.attr("rel-addr"), c.attr("rel-port")) {
        s += &format!(" raddr {a} rport {p}");
    }
    s += &format!(" generation {}", c.attr("generation").unwrap_or("0"));
    Some(s)
}

/// The peer's contents as one SDP. Contents added later may carry an empty transport: ICE and
/// DTLS come from the first one that has them (BUNDLE). `peer_role` maps `senders` to directions.
fn contents_to_sdp(contents: &[Element], peer_role: &str) -> String {
    let mut s = String::from("v=0\r\no=- 1 2 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\n");
    let names: Vec<&str> = contents.iter().filter_map(|c| c.attr("name")).collect();
    s += &format!("a=group:BUNDLE {}\r\na=msid-semantic: WMS *\r\n", names.join(" "));
    let transports = || contents.iter().filter_map(|c| c.get_child("transport", NS_ICE));
    let ice = transports().find(|t| t.attr("ufrag").is_some_and(|u| !u.is_empty()));
    let fp = transports().find_map(|t| t.get_child("fingerprint", NS_DTLS));
    for (i, content) in contents.iter().enumerate() {
        let Some(desc) = content.get_child("description", NS_RTP) else { continue };
        let media = desc.attr("media").unwrap_or("audio");
        let pts: Vec<&Element> = desc.children().filter(|e| e.is("payload-type", NS_RTP)).collect();
        let ids: Vec<&str> = pts.iter().filter_map(|p| p.attr("id")).collect();
        s += &format!("m={media} 9 UDP/TLS/RTP/SAVPF {}\r\nc=IN IP4 0.0.0.0\r\na=rtcp:9 IN IP4 0.0.0.0\r\n", ids.join(" "));
        if let Some(t) = ice {
            s += &format!("a=ice-ufrag:{}\r\na=ice-pwd:{}\r\n", t.attr("ufrag").unwrap_or_default(), t.attr("pwd").unwrap_or_default());
        }
        if let Some(fp) = fp {
            s += &format!("a=fingerprint:{} {}\r\na=setup:{}\r\n", fp.attr("hash").unwrap_or("sha-256"), fp.text().trim(), fp.attr("setup").unwrap_or("actpass"));
        }
        if i == 0 {
            for c in transports().flat_map(|t| t.children().filter(|e| e.is("candidate", NS_ICE))) {
                if let Some(line) = jingle_to_candidate(c) {
                    s += &format!("a={line}\r\n");
                }
            }
        }
        s += &format!("a=mid:{}\r\n", content.attr("name").unwrap_or("0"));
        let dir = match content.attr("senders").unwrap_or("both") {
            "both" => "sendrecv",
            "none" => "inactive",
            r if r == peer_role => "sendonly",
            _ => "recvonly",
        };
        s += &format!("a={dir}\r\n");
        // Bundled media is always rtcp-muxed (Conversations adds it the same way).
        if desc.get_child("rtcp-mux", NS_RTP).is_some() || contents.len() > 1 {
            s += "a=rtcp-mux\r\n";
        }
        for e in desc.children().filter(|e| e.is("rtp-hdrext", NS_HDREXT)) {
            s += &format!("a=extmap:{} {}\r\n", e.attr("id").unwrap_or_default(), e.attr("uri").unwrap_or_default());
        }
        for p in pts {
            let id = p.attr("id").unwrap_or_default();
            let ch = p.attr("channels").filter(|c| *c != "1" && media == "audio").map(|c| format!("/{c}")).unwrap_or_default();
            let rate = p.attr("clockrate").unwrap_or(if media == "video" { "90000" } else { "8000" });
            s += &format!("a=rtpmap:{id} {}/{rate}{ch}\r\n", p.attr("name").unwrap_or_default());
            let params: Vec<String> = p
                .children()
                .filter(|e| e.is("parameter", NS_RTP))
                .map(|e| format!("{}={}", e.attr("name").unwrap_or_default(), e.attr("value").unwrap_or_default()))
                .collect();
            if !params.is_empty() {
                s += &format!("a=fmtp:{id} {}\r\n", params.join(";"));
            }
            for fb in p.children().filter(|e| e.is("rtcp-fb", NS_FB)) {
                let sub = fb.attr("subtype").map(|x| format!(" {x}")).unwrap_or_default();
                s += &format!("a=rtcp-fb:{id} {}{sub}\r\n", fb.attr("type").unwrap_or_default());
            }
        }
        for src in desc.children().filter(|e| e.is("source", NS_SSMA)) {
            let ssrc = src.attr("ssrc").unwrap_or_default();
            for p in src.children().filter(|e| e.name() == "parameter") {
                let v = p.attr("value").map(|v| format!(":{v}")).unwrap_or_default();
                s += &format!("a=ssrc:{ssrc} {}{v}\r\n", p.attr("name").unwrap_or_default());
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFER: &str = "v=0\r\no=- 123 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0 1\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111 0\r\nc=IN IP4 0.0.0.0\r\na=ice-ufrag:uf\r\na=ice-pwd:pw\r\n\
a=fingerprint:sha-256 AA:BB\r\na=setup:actpass\r\na=mid:0\r\na=sendrecv\r\na=rtcp-mux\r\n\
a=rtpmap:111 opus/48000/2\r\na=fmtp:111 minptime=10;useinbandfec=1\r\na=rtcp-fb:111 transport-cc\r\n\
a=rtpmap:0 PCMU/8000\r\na=ssrc:42 cname:abc\r\n\
a=candidate:1 1 udp 2130706431 192.168.1.5 50000 typ host generation 0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 96\r\nc=IN IP4 0.0.0.0\r\na=ice-ufrag:uf\r\na=ice-pwd:pw\r\n\
a=fingerprint:sha-256 AA:BB\r\na=setup:actpass\r\na=mid:1\r\na=sendonly\r\na=rtcp-mux\r\n\
a=rtpmap:96 VP8/90000\r\na=rtcp-fb:96 nack pli\r\na=ssrc:43 cname:abc\r\n";

    fn session(p: &ParsedSdp, initiator: bool) -> Vec<Element> {
        p.sections.iter().enumerate().map(|(i, s)| sdp_to_content(p, s, initiator, i == 0)).collect()
    }

    #[test]
    fn sdp_jingle_roundtrip_keeps_what_webrtc_needs() {
        let p = parse_sdp(OFFER);
        assert_eq!((p.ufrag.as_str(), p.pwd.as_str(), p.setup.as_str()), ("uf", "pw", "actpass"));
        assert_eq!(p.sections.iter().map(|s| (s.media.as_str(), s.mid.as_str())).collect::<Vec<_>>(), [("audio", "0"), ("video", "1")]);
        assert_eq!(p.sections[0].payloads.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["111", "0"]);

        let contents = session(&p, true);
        assert_eq!(contents[1].attr("senders"), Some("initiator"));
        let back = contents_to_sdp(&contents, "initiator");
        for needle in [
            "a=group:BUNDLE 0 1",
            "m=audio 9 UDP/TLS/RTP/SAVPF 111 0",
            "a=ice-ufrag:uf",
            "a=ice-pwd:pw",
            "a=fingerprint:sha-256 AA:BB",
            "a=setup:actpass",
            "a=mid:0",
            "a=rtcp-mux",
            "a=rtpmap:111 opus/48000/2",
            "a=fmtp:111 minptime=10;useinbandfec=1",
            "a=rtcp-fb:111 transport-cc",
            "a=rtpmap:0 PCMU/8000",
            "a=ssrc:42 cname:abc",
            "a=candidate:1 1 udp 2130706431 192.168.1.5 50000 typ host generation 0",
            "m=video 9 UDP/TLS/RTP/SAVPF 96",
            "a=mid:1\r\na=sendonly",
            "a=rtpmap:96 VP8/90000",
            "a=rtcp-fb:96 nack pli",
        ] {
            assert!(back.contains(needle), "missing {needle:?} in\n{back}");
        }
        // Seen from the other side, the peer's sendonly video is ours to receive.
        assert!(contents_to_sdp(&contents, "responder").contains("a=mid:1\r\na=recvonly"));
    }

    #[test]
    fn added_content_with_empty_transport_inherits_ice() {
        let p = parse_sdp(OFFER);
        let mut contents = session(&p, true);
        let video = Element::builder("content", NS_JINGLE)
            .attr(nc("creator"), "initiator")
            .attr(nc("name"), "v")
            .append(contents[1].get_child("description", NS_RTP).unwrap().clone())
            .append(Element::builder("transport", NS_ICE))
            .build();
        contents.truncate(1);
        contents.push(video);
        let sdp = contents_to_sdp(&contents, "initiator");
        let video_part = &sdp[sdp.find("m=video").unwrap()..];
        assert!(video_part.contains("a=ice-ufrag:uf") && video_part.contains("a=fingerprint:sha-256 AA:BB"), "{sdp}");
        assert!(video_part.contains("a=mid:v\r\na=sendrecv"), "{sdp}");
    }

    struct TestHandler(UnboundedSender<RTCIceCandidateInit>, UnboundedSender<Arc<dyn TrackRemote>>, UnboundedSender<RTCPeerConnectionState>);

    #[async_trait::async_trait]
    impl PeerConnectionEventHandler for TestHandler {
        async fn on_ice_candidate(&self, event: RTCPeerConnectionIceEvent) {
            if let Ok(init) = event.candidate.to_json() {
                let _ = self.0.unbounded_send(init);
            }
        }
        async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
            let _ = self.2.unbounded_send(state);
        }
        async fn on_track(&self, track: Arc<dyn TrackRemote>) {
            let _ = self.1.unbounded_send(track);
        }
    }

    type Rx<T> = futures::channel::mpsc::UnboundedReceiver<T>;

    type TestPc = (Arc<dyn PeerConnection>, Rx<RTCIceCandidateInit>, Rx<Arc<dyn TrackRemote>>, Rx<RTCPeerConnectionState>, Arc<TrackLocalStaticRTP>);

    /// A peer connection set up like a call: our codecs and an audio track we write RTP into.
    async fn test_pc() -> TestPc {
        test_pc_with(media_engine().unwrap()).await
    }

    async fn test_pc_with(mut media: MediaEngine) -> TestPc {
        let registry = register_default_interceptors(Registry::new(), &mut media).unwrap();
        let (c, cr) = unbounded();
        let (t, tr) = unbounded();
        let (s, sr) = unbounded();
        let pc = PeerConnectionBuilder::new()
            .with_configuration(RTCConfigurationBuilder::new().build())
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_handler(Arc::new(TestHandler(c, t, s)))
            .with_udp_addrs(vec!["127.0.0.1:0"])
            .build()
            .await
            .unwrap();
        let pc: Arc<dyn PeerConnection> = Arc::new(pc);
        let track = Arc::new(audio_track("t", rand::random()));
        pc.add_track(track.clone() as Arc<dyn TrackLocal>).await.unwrap();
        (pc, cr, tr, sr, track)
    }

    /// Offer from `a` (initiator) and answer from `b`, both through our Jingle translation; trickles
    /// the candidates and waits until `b` connects. Returns the SDP `a` set as the remote answer.
    async fn connect(a: &mut TestPc, b: &mut TestPc) -> String {
        a.0.set_local_description(a.0.create_offer(None).await.unwrap()).await.unwrap();
        let offer = session(&parse_sdp(&local_sdp(&a.0).await), true);
        b.0.set_remote_description(RTCSessionDescription::offer(contents_to_sdp(&offer, "initiator")).unwrap()).await.unwrap();
        b.0.set_local_description(b.0.create_answer(None).await.unwrap()).await.unwrap();
        let answer = contents_to_sdp(&session(&parse_sdp(&local_sdp(&b.0).await), false), "responder");
        a.0.set_remote_description(RTCSessionDescription::answer(answer.clone()).unwrap()).await.unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        while let Ok(c) = a.1.try_recv() {
            let _ = b.0.add_ice_candidate(c).await;
        }
        while let Ok(c) = b.1.try_recv() {
            let _ = a.0.add_ice_candidate(c).await;
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(s) = b.3.next().await {
                if s == RTCPeerConnectionState::Connected {
                    break;
                }
            }
        })
        .await
        .expect("call connects");
        answer
    }

    /// Sends a tone from `a` as the real call path does and returns the first audio packet `b` gets.
    async fn send_tone(a: &TestPc, b: &mut TestPc, format: AudioFormat) -> rtc::rtp::Packet {
        let (tx, rx) = unbounded::<Vec<f32>>();
        let writer = tokio::spawn(write_audio(rx, a.4.clone(), a.4.track().await.ssrcs().next().unwrap(), Arc::new(Mutex::new(format)), Arc::default()));
        let feeder = tokio::spawn(async move {
            for i in 0.. {
                let pcm = (0..audio::CALL_FRAME).map(|n| ((i * audio::CALL_FRAME + n) as f32 * 440.0 * std::f32::consts::TAU / 48000.0).sin() * 0.5).collect();
                if tx.unbounded_send(pcm).is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });
        let packet = tokio::time::timeout(Duration::from_secs(10), async {
            let track = b.2.next().await.expect("audio track");
            loop {
                if let Some(TrackRemoteEvent::OnRtpPacket(p)) = track.poll().await {
                    break p;
                }
            }
        })
        .await
        .expect("audio packet");
        writer.abort();
        feeder.abort();
        packet
    }

    /// Both sides have Opus: it is negotiated and a tone sent as Opus decodes on the far side.
    #[tokio::test(flavor = "multi_thread")]
    async fn audio_goes_as_opus() {
        let (mut a, mut b) = (test_pc().await, test_pc().await);
        let answer = connect(&mut a, &mut b).await;
        let format = formats(&answer).0.unwrap();
        assert_eq!(format, AudioFormat { opus: true, pt: OPUS_PT });
        let p = send_tone(&a, &mut b, format).await;
        assert_eq!(p.header.payload_type, OPUS_PT);
        let pcm = audio::OpusDecoder::new().unwrap().decode(&p.payload);
        assert_eq!(pcm.len(), audio::CALL_FRAME);
        let _ = (a.0.close().await, b.0.close().await);
    }

    /// A peer with PCMU only (older clients): the call falls back to it.
    #[tokio::test(flavor = "multi_thread")]
    async fn audio_falls_back_to_pcmu() {
        let mut only_pcmu = MediaEngine::default();
        only_pcmu.register_codec(RTCRtpCodecParameters { rtp_codec: pcmu(), payload_type: 0 }, RtpCodecKind::Audio).unwrap();
        let (mut a, mut b) = (test_pc().await, test_pc_with(only_pcmu).await);
        let answer = connect(&mut a, &mut b).await;
        let format = formats(&answer).0.unwrap();
        assert_eq!(format, AudioFormat { opus: false, pt: 0 });
        let p = send_tone(&a, &mut b, format).await;
        assert_eq!((p.header.payload_type, p.payload.len()), (0, audio::FRAME));
        let _ = (a.0.close().await, b.0.close().await);
    }

    fn vp8_track(ssrc: u32) -> Arc<TrackLocalStaticSample> {
        let enc = RTCRtpEncodingParameters { rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(ssrc), ..Default::default() }, codec: vp8(), ..Default::default() };
        Arc::new(TrackLocalStaticSample::new(Instant::now(), MediaStreamTrack::new("t".into(), "v".into(), "v".into(), RtpCodecKind::Video, vec![enc])).unwrap())
    }

    async fn local_sdp(pc: &Arc<dyn PeerConnection>) -> String {
        pc.local_description().await.unwrap().sdp
    }

    /// An audio call between two real peer connections, upgraded to video with content-add /
    /// content-accept through our Jingle translation; VP8 from ffmpeg must reach the far decoder.
    #[tokio::test(flavor = "multi_thread")]
    async fn audio_call_upgrades_to_video_over_jingle() {
        if !crate::video::available() {
            eprintln!("ffmpeg not found, skipping");
            return;
        }
        let (a, mut a_cands, _a_tracks, _, _) = test_pc().await;
        let (b, mut b_cands, mut b_tracks, mut b_state, _) = test_pc().await;

        // session-initiate / session-accept: A is the initiator.
        a.set_local_description(a.create_offer(None).await.unwrap()).await.unwrap();
        let pa = parse_sdp(&local_sdp(&a).await);
        let a_contents = session(&pa, true);
        b.set_remote_description(RTCSessionDescription::offer(contents_to_sdp(&a_contents, "initiator")).unwrap()).await.unwrap();
        b.set_local_description(b.create_answer(None).await.unwrap()).await.unwrap();
        let pb = parse_sdp(&local_sdp(&b).await);
        let mut b_contents = session(&pb, false);
        a.set_remote_description(RTCSessionDescription::answer(contents_to_sdp(&b_contents, "responder")).unwrap()).await.unwrap();

        // Trickle the host candidates both ways.
        tokio::time::sleep(Duration::from_millis(500)).await;
        while let Ok(c) = a_cands.try_recv() {
            let _ = b.add_ice_candidate(c).await;
        }
        while let Ok(c) = b_cands.try_recv() {
            let _ = a.add_ice_candidate(c).await;
        }
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(s) = b_state.next().await {
                if s == RTCPeerConnectionState::Connected {
                    break;
                }
            }
        })
        .await
        .expect("audio call connects");

        // A turns the camera on: content-add with only the new m-line.
        let ssrc = 4242;
        let track = vp8_track(ssrc);
        a.add_track(track.clone() as Arc<dyn TrackLocal>).await.unwrap();
        a.set_local_description(a.create_offer(None).await.unwrap()).await.unwrap();
        let pa = parse_sdp(&local_sdp(&a).await);
        let known: Vec<String> = a_contents.iter().filter_map(|c| c.attr("name")).map(str::to_string).collect();
        let added: Vec<Element> = pa.sections.iter().filter(|s| !known.contains(&s.mid)).map(|s| sdp_to_content(&pa, s, true, false)).collect();
        assert_eq!(added.len(), 1);
        assert_eq!(media_of(&added[0]), Some("video"));

        // B accepts: full offer from what it knows of A plus the addition, answered with its own video track.
        let mut remote_a = a_contents.clone();
        remote_a.extend(added.iter().cloned());
        b.set_remote_description(RTCSessionDescription::offer(contents_to_sdp(&remote_a, "initiator")).unwrap()).await.unwrap();
        b.add_track(vp8_track(777) as Arc<dyn TrackLocal>).await.unwrap();
        b.set_local_description(b.create_answer(None).await.unwrap()).await.unwrap();
        let pb = parse_sdp(&local_sdp(&b).await);
        let mid = added[0].attr("name").unwrap().to_string();
        let accepted: Vec<Element> = pb.sections.iter().filter(|s| s.mid == mid).map(|s| sdp_to_content(&pb, s, false, false)).collect();
        assert_eq!(accepted.len(), 1);
        assert_eq!(accepted[0].attr("senders"), Some("both"));
        b_contents.extend(accepted);
        a.set_remote_description(RTCSessionDescription::answer(contents_to_sdp(&b_contents, "responder")).unwrap()).await.unwrap();

        // A streams a test pattern encoded by ffmpeg; B decodes it.
        let (frames_tx, mut frames_rx) = unbounded::<Vec<u8>>();
        let mut input: Vec<String> = ["-re", "-f", "lavfi", "-i", "testsrc=size=640x360:rate=30"].iter().map(|s| s.to_string()).collect();
        input.extend(crate::videocall::encode_args(Source::Camera));
        let mut enc = crate::videocall::spawn_encoder(&input, Arc::new(Mutex::new(Box::new(move |f| {
            let _ = frames_tx.unbounded_send(f);
        }))))
        .unwrap();
        let writer = tokio::spawn(async move {
            while let Some(f) = frames_rx.next().await {
                let sample = Sample {
                    data: bytes::Bytes::from(f),
                    timestamp: Instant::now(),
                    duration: Duration::from_millis(33),
                    packet_timestamp: 0,
                    prev_dropped_packets: 0,
                    prev_padding_packets: 0,
                };
                let _ = track.write_sample(ssrc, VP8_PT, &sample, &[]).await;
            }
        });
        let got = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let t = b_tracks.next().await.expect("track");
                if t.kind().await == RtpCodecKind::Video {
                    tokio::spawn(receive_video(t));
                    break;
                }
            }
            loop {
                let done = {
                    let slot = FRAMES.remote.lock().unwrap();
                    slot.frame.as_ref().is_some_and(|f| slot.size == (640, 360) && f.len() == 640 * 360 * 4)
                };
                if done {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        let _ = enc.kill();
        let _ = enc.wait();
        writer.abort();
        let _ = a.close().await;
        let _ = b.close().await;
        got.expect("decoded video frame on the far side");
    }

    fn jingle_el(action: &str, sid: &str, payload: Option<Element>) -> Element {
        let mut b = Element::builder("jingle", NS_JINGLE).attr(nc("action"), action).attr(nc("sid"), sid);
        if let Some(p) = payload {
            b = b.append(p);
        }
        b.build()
    }

    /// XEP-0166 §10 / XEP-0167 §7: what a request is refused with.
    #[test]
    fn jingle_requests_are_checked() {
        let info = |name: &str, ns: &str| Some(Element::builder(name, ns).build());
        let session = |initiator, adding, awaiting_accept| Session { sid: Some("s1".into()), initiator, adding, awaiting_accept };
        let err = |el: Element, tie: bool| check(&el, &session(true, tie, true)).err().map(|e| (e.condition, e.jingle));
        assert_eq!(err(jingle_el("session-initiate", "new", None), false), None);
        assert_eq!(err(jingle_el("transport-info", "s1", None), false), None);
        assert_eq!(err(jingle_el("transport-info", "other", None), false), Some((DefinedCondition::ItemNotFound, Some("unknown-session"))));
        assert_eq!(err(jingle_el("dance", "s1", None), false), Some((DefinedCondition::BadRequest, None)));
        // An empty session-info is a ping; RTP info payloads are known, anything else is not.
        assert_eq!(err(jingle_el("session-info", "s1", None), false), None);
        assert_eq!(err(jingle_el("session-info", "s1", info("mute", NS_RTP_INFO)), false), None);
        assert_eq!(err(jingle_el("session-info", "s1", info("ringing", NS_RTP_INFO)), false), None);
        assert_eq!(err(jingle_el("session-info", "s1", info("transfer", "urn:xmpp:jingle:transfer:0")), false), Some((DefinedCondition::FeatureNotImplemented, Some("unsupported-info"))));
        // Our own content-add as initiator is pending: theirs loses the tie.
        assert_eq!(err(jingle_el("content-add", "s1", None), false), None);
        assert_eq!(err(jingle_el("content-add", "s1", None), true), Some((DefinedCondition::Conflict, Some("tie-break"))));
        // Answers to requests we did not make (§7.2.16 out-of-order); a responder's own add does not win.
        let out_of_order = Some((DefinedCondition::UnexpectedRequest, Some("out-of-order")));
        let got = |el: Element, s: Session| check(&el, &s).err().map(|e| (e.condition, e.jingle));
        assert_eq!(got(jingle_el("session-accept", "s1", None), session(false, false, false)), out_of_order);
        assert_eq!(got(jingle_el("content-accept", "s1", None), session(true, false, true)), out_of_order);
        assert_eq!(got(jingle_el("content-add", "s1", None), session(false, true, false)), None);
        let e = check(&jingle_el("session-info", "s1", info("x", "y")), &session(true, false, false)).unwrap_err();
        assert!(e.retry());
        assert!(e.jingle_element().unwrap().is("unsupported-info", NS_JINGLE_ERRORS));
    }

    #[test]
    fn negotiated_formats() {
        let sdp = "m=audio 9 UDP/TLS/RTP/SAVPF 109 0\r\na=rtpmap:109 opus/48000/2\r\na=rtpmap:0 PCMU/8000\r\nm=video 9 UDP/TLS/RTP/SAVPF 100\r\na=rtpmap:100 VP8/90000\r\n";
        assert_eq!(formats(sdp), (Some(AudioFormat { opus: true, pt: 109 }), Some(100)));
        let pcmu_only = "m=audio 9 UDP/TLS/RTP/SAVPF 0\r\na=rtpmap:0 PCMU/8000\r\n";
        assert_eq!(formats(pcmu_only), (Some(AudioFormat { opus: false, pt: 0 }), None));
    }

    #[test]
    fn candidate_with_related_address() {
        let el = candidate_to_jingle("candidate:2 1 UDP 1694498815 1.2.3.4 6000 typ srflx raddr 10.0.0.2 rport 5000").unwrap();
        assert_eq!(el.attr("type"), Some("srflx"));
        assert_eq!(el.attr("protocol"), Some("udp"));
        assert_eq!(jingle_to_candidate(&el).unwrap(), "candidate:2 1 udp 1694498815 1.2.3.4 6000 typ srflx raddr 10.0.0.2 rport 5000 generation 0");
    }
}
