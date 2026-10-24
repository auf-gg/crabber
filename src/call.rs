//! Audio calls: XEP-0353 (Jingle Message Initiation) for ringing, XEP-0166/0167/0176/0320
//! (Jingle RTP over ICE-UDP with DTLS-SRTP) for the session, webrtc-rs for the media stack.
//! Signalling is translated between Jingle XML and the SDP that webrtc-rs speaks.
use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedSender, unbounded};
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_PCMU, MediaEngine};
use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind};
use tokio_xmpp::IqRequest;
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::message::{Message, MessageType};
use tokio_xmpp::parsers::stanza_error::DefinedCondition;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceCandidateInit, RTCIceServer,
    RTCPeerConnectionIceEvent, RTCPeerConnectionState, RTCSessionDescription, Registry, register_default_interceptors,
};

use crate::audio::{self, Audio};
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

/// Received audio RTP packets across all calls; lets tests and diagnostics see that media flows.
pub static RTP_RECEIVED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub enum CallCmd {
    Start { to: String },
    Accept,
    Hangup,
    Mute(bool),
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
}

enum Msg {
    Cmd(CallCmd),
    Jingle(Jid, Element),
    Jmi(Jid, Element),
    Candidate(String, RTCIceCandidateInit),
    PcState(String, RTCPeerConnectionState),
    Track(String, Arc<dyn TrackRemote>),
    Timeout(String),
}

/// Front end owned by the XMPP loop; the call itself runs in its own task.
pub struct Calls {
    tx: UnboundedSender<Msg>,
    me: BareJid,
    /// Current session id, for synchronous IQ validation.
    sid: Arc<Mutex<Option<String>>>,
    bound: Arc<Mutex<Option<Jid>>>,
}

impl Calls {
    pub fn new(net: Net, me: BareJid) -> Self {
        let (tx, mut rx) = unbounded();
        let sid = Arc::new(Mutex::new(None));
        let bound = Arc::new(Mutex::new(None));
        let mut actor = Actor { net, me: me.clone(), tx: tx.clone(), call: None, sid: sid.clone(), bound: bound.clone() };
        tokio::spawn(async move {
            while let Some(msg) = rx.next().await {
                actor.handle(msg).await;
            }
        });
        Self { tx, me, sid, bound }
    }

    pub fn set_bound(&self, jid: Jid) {
        *self.bound.lock().unwrap() = Some(jid);
    }

    pub fn on_cmd(&mut self, cmd: CallCmd) {
        let _ = self.tx.unbounded_send(Msg::Cmd(cmd));
    }

    /// Validates synchronously so the IQ can be answered; the work happens in the actor.
    pub fn on_jingle(&self, from: Option<Jid>, el: Element) -> Result<(), DefinedCondition> {
        let from = from.ok_or(DefinedCondition::BadRequest)?;
        let action = el.attr("action").unwrap_or_default();
        let sid = el.attr("sid").unwrap_or_default();
        if action != "session-initiate" && self.sid.lock().unwrap().as_deref() != Some(sid) {
            return Err(DefinedCondition::ItemNotFound);
        }
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
    content: String,
    ufrag: String,
    audio: Option<Arc<Audio>>,
    /// Ring / ringback while the call is not answered yet.
    tone: Option<audio::Tone>,
}

struct Actor {
    net: Net,
    me: BareJid,
    tx: UnboundedSender<Msg>,
    call: Option<Call>,
    sid: Arc<Mutex<Option<String>>>,
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

impl Actor {
    fn emit(&self, state: CallState) {
        if let Some(c) = &self.call {
            self.net.emit(Ev::Call(CallEv::State { peer: c.peer_bare.to_string(), state }));
        }
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
        *self.sid.lock().unwrap() = None;
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
                    RTCPeerConnectionState::Connected => self.set_state(CallState::Active),
                    RTCPeerConnectionState::Failed => {
                        self.terminate("failed-transport").await;
                        self.end("не удалось установить соединение").await;
                    }
                    _ => {}
                }
            }
            Msg::Track(sid, track) => {
                let Some(audio) = self.call.as_ref().filter(|c| c.sid == sid).and_then(|c| c.audio.clone()) else { return };
                tokio::spawn(async move {
                    while let Some(ev) = track.poll().await {
                        if let TrackRemoteEvent::OnRtpPacket(p) = ev
                            && p.header.payload_type == 0
                        {
                            RTP_RECEIVED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            audio.play_ulaw(&p.payload);
                        }
                    }
                });
            }
            Msg::Timeout(sid) => {
                if let Some(c) = self.call.as_ref().filter(|c| c.sid == sid && c.state == CallState::Outgoing) {
                    self.net.send(chat_with(c.peer_bare.clone().into(), jmi("retract", &sid)));
                    self.end("нет ответа").await;
                }
            }
        }
    }

    async fn on_cmd(&mut self, cmd: CallCmd) {
        match cmd {
            CallCmd::Start { to } => {
                if self.call.is_some() {
                    return;
                }
                let Ok(peer_bare) = BareJid::from_str(&to) else { return };
                let sid = crate::xmpp::new_id();
                let propose = Element::builder("propose", NS_JMI)
                    .attr(nc("id"), sid.as_str())
                    .append(Element::builder("description", NS_RTP).attr(nc("media"), "audio"))
                    .build();
                self.net.send(chat_with(peer_bare.clone().into(), propose));
                self.start_call(sid.clone(), peer_bare, None, true, CallState::Outgoing);
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    let _ = tx.unbounded_send(Msg::Timeout(sid));
                });
            }
            CallCmd::Accept => {
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
                    _ => self.terminate("success").await,
                }
                self.end("").await;
            }
            CallCmd::Mute(m) => {
                if let Some(a) = self.call.as_ref().and_then(|c| c.audio.as_ref()) {
                    a.muted.store(m, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    }

    fn start_call(&mut self, sid: String, peer_bare: BareJid, peer: Option<Jid>, initiator: bool, state: CallState) {
        *self.sid.lock().unwrap() = Some(sid.clone());
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
            content: "0".into(),
            ufrag: String::new(),
            audio: None,
            tone: Some(audio::tone(state == CallState::Incoming)),
        });
        self.emit(state);
    }

    async fn on_jmi(&mut self, from: Jid, el: Element) {
        let Some(sid) = el.attr("id").map(str::to_string) else { return };
        let from_self = from.to_bare() == self.me;
        match el.name() {
            "propose" if !from_self => {
                let audio = el.children().any(|d| d.is("description", NS_RTP) && d.attr("media") == Some("audio"));
                if !audio {
                    return;
                }
                if self.call.is_some() {
                    if self.call.as_ref().is_some_and(|c| c.sid == sid) {
                        return;
                    }
                    self.net.send(chat_with(from, jmi("reject", &sid)));
                    return;
                }
                self.start_call(sid, from.to_bare(), Some(from), false, CallState::Incoming);
            }
            "retract" if self.call.as_ref().is_some_and(|c| c.sid == sid) => self.end("звонящий отменил вызов").await,
            "reject" if self.call.as_ref().is_some_and(|c| c.sid == sid) => {
                let reason = if from_self { "отклонён на другом устройстве" } else { "собеседник отклонил вызов" };
                self.end(reason).await;
            }
            "accept" if from_self && self.call.as_ref().is_some_and(|c| c.sid == sid && c.state == CallState::Incoming) => {
                self.end("принят на другом устройстве").await;
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
                    self.start_call(sid.clone(), from.to_bare(), Some(from.clone()), false, CallState::Incoming);
                }
                let c = self.call.as_mut().unwrap();
                c.peer = Some(from);
                if c.state == CallState::Connecting {
                    self.answer(el).await;
                } else {
                    c.pending_offer = Some(el);
                }
            }
            "session-accept" => {
                let Some(c) = self.call.as_ref() else { return };
                let Some(pc) = c.pc.clone() else { return };
                let ok = match RTCSessionDescription::answer(jingle_to_sdp(&el)) {
                    Ok(desc) => pc.set_remote_description(desc).await.is_ok(),
                    Err(_) => false,
                };
                match ok {
                    true => self.flush_candidates(&pc).await,
                    false => {
                        self.terminate("failed-application").await;
                        self.end("несовместимые параметры звонка").await;
                    }
                }
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
                    "busy" => "собеседник занят",
                    "decline" => "собеседник отклонил вызов",
                    "success" | "" => "",
                    _ => "соединение прервано",
                };
                self.end(text).await;
            }
            _ => {}
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

    fn content(&self, c: &Call, child: tokio_xmpp::minidom::ElementBuilder) -> tokio_xmpp::minidom::ElementBuilder {
        let _ = c.initiator;
        Element::builder("content", NS_JINGLE).attr(nc("creator"), "initiator").attr(nc("name"), c.content.as_str()).append(child)
    }

    fn iq(&self, to: Jid, el: Element) {
        let net = self.net.clone();
        tokio::spawn(async move {
            let _ = net.iq(Some(to), IqRequest::Set(el)).await;
        });
    }

    async fn terminate(&mut self, why: &str) {
        let Some(c) = self.call.as_ref() else { return };
        let Some(peer) = c.peer.clone() else { return };
        let el = self.jingle("session-terminate", &c.sid).append(reason(why)).build();
        self.iq(peer, el);
    }

    /// Builds the peer connection, local audio track and audio devices.
    async fn setup_pc(&mut self) -> Result<Arc<dyn PeerConnection>, String> {
        let sid = self.call.as_ref().ok_or("нет звонка")?.sid.clone();
        let ice_servers = ice_servers(&self.net, self.me.domain().as_str()).await;

        let mut media = MediaEngine::default();
        let pcmu = RTCRtpCodec { mime_type: MIME_TYPE_PCMU.into(), clock_rate: 8000, channels: 1, sdp_fmtp_line: String::new(), rtcp_feedback: vec![] };
        // shortcut: only PCMU is negotiated; add Opus once a pure-Rust encoder is available.
        media
            .register_codec(RTCRtpCodecParameters { rtp_codec: pcmu.clone(), payload_type: 0 }, RtpCodecKind::Audio)
            .map_err(|e| e.to_string())?;
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
        let track = Arc::new(
            TrackLocalStaticSample::new(
                Instant::now(),
                MediaStreamTrack::new(
                    "crabber".into(),
                    format!("audio-{sid}"),
                    "audio".into(),
                    RtpCodecKind::Audio,
                    vec![RTCRtpEncodingParameters {
                        rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(ssrc), ..Default::default() },
                        codec: pcmu,
                        ..Default::default()
                    }],
                ),
            )
            .map_err(|e| e.to_string())?,
        );
        pc.add_track(track.clone() as Arc<dyn TrackLocal>).await.map_err(|e| e.to_string())?;

        let (frames_tx, mut frames_rx) = unbounded::<Vec<u8>>();
        let audio = audio::start(move |frame| {
            let _ = frames_tx.unbounded_send(frame);
        })?;
        tokio::spawn(async move {
            while let Some(frame) = frames_rx.next().await {
                let sample = Sample {
                    data: bytes::Bytes::from(frame),
                    timestamp: Instant::now(),
                    duration: Duration::from_millis(20),
                    packet_timestamp: 0,
                    prev_dropped_packets: 0,
                    prev_padding_packets: 0,
                };
                // Errors before the transport is up are expected; frames are just dropped.
                let _ = track.write_sample(ssrc, 0, &sample, &[]).await;
            }
        });
        let c = self.call.as_mut().ok_or("нет звонка")?;
        c.audio = Some(Arc::new(audio));
        c.pc = Some(pc.clone());
        Ok(pc)
    }

    async fn offer(&mut self) {
        let pc = match self.setup_pc().await {
            Ok(pc) => pc,
            Err(e) => return self.fail(&e).await,
        };
        let res = async {
            let offer = pc.create_offer(None).await.map_err(|e| e.to_string())?;
            pc.set_local_description(offer).await.map_err(|e| e.to_string())?;
            pc.local_description().await.ok_or("нет SDP".to_string())
        }
        .await;
        match res {
            Ok(local) => self.send_description("session-initiate", &local.sdp),
            Err(e) => self.fail(&e).await,
        }
    }

    async fn answer(&mut self, offer: Element) {
        let pc = match self.setup_pc().await {
            Ok(pc) => pc,
            Err(e) => return self.fail(&e).await,
        };
        if let Some(name) = offer.get_child("content", NS_JINGLE).and_then(|c| c.attr("name"))
            && let Some(c) = self.call.as_mut()
        {
            c.content = name.to_string();
        }
        let res = async {
            let desc = RTCSessionDescription::offer(jingle_to_sdp(&offer)).map_err(|e| e.to_string())?;
            pc.set_remote_description(desc).await.map_err(|e| e.to_string())?;
            let answer = pc.create_answer(None).await.map_err(|e| e.to_string())?;
            pc.set_local_description(answer).await.map_err(|e| e.to_string())?;
            pc.local_description().await.ok_or("нет SDP".to_string())
        }
        .await;
        match res {
            Ok(local) => {
                self.flush_candidates(&pc).await;
                self.send_description("session-accept", &local.sdp);
            }
            Err(e) => self.fail(&e).await,
        }
    }

    fn send_description(&mut self, action: &str, sdp: &str) {
        let Some(c) = self.call.as_mut() else { return };
        let Some(peer) = c.peer.clone() else { return };
        let parsed = parse_sdp(sdp);
        if action == "session-initiate" {
            c.content = parsed.mid.clone();
        }
        c.ufrag = parsed.ufrag.clone();
        let sid = c.sid.clone();
        let content = sdp_to_content(&parsed, &c.content);
        let group = Element::builder("group", NS_GROUP)
            .attr(nc("semantics"), "BUNDLE")
            .append(Element::builder("content", NS_GROUP).attr(nc("name"), c.content.as_str()));
        let el = self.jingle(action, &sid).append(content).append(group).build();
        self.iq(peer, el);
    }

    async fn fail(&mut self, error: &str) {
        self.terminate("failed-application").await;
        self.end(&format!("ошибка: {error}")).await;
    }
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
struct ParsedSdp {
    mid: String,
    ufrag: String,
    pwd: String,
    fingerprint: Option<(String, String)>,
    setup: String,
    /// id -> (name, clockrate, channels)
    payloads: Vec<(String, String, String, String)>,
    fmtp: BTreeMap<String, Vec<(String, String)>>,
    rtcp_fb: BTreeMap<String, Vec<(String, Option<String>)>>,
    extmap: Vec<(String, String)>,
    ssrc: BTreeMap<String, Vec<(String, String)>>,
    rtcp_mux: bool,
    candidates: Vec<String>,
}

fn parse_sdp(sdp: &str) -> ParsedSdp {
    let mut p = ParsedSdp { mid: "0".into(), ..Default::default() };
    let mut order = vec![];
    for line in sdp.lines().map(str::trim) {
        if let Some(m) = line.strip_prefix("m=audio ") {
            order = m.split_whitespace().skip(2).map(str::to_string).collect();
        } else if let Some(a) = line.strip_prefix("a=") {
            let (key, val) = a.split_once(':').unwrap_or((a, ""));
            match key {
                "mid" => p.mid = val.to_string(),
                "ice-ufrag" => p.ufrag = val.to_string(),
                "ice-pwd" => p.pwd = val.to_string(),
                "setup" => p.setup = val.to_string(),
                "rtcp-mux" => p.rtcp_mux = true,
                "fingerprint" => {
                    if let Some((h, f)) = val.split_once(' ') {
                        p.fingerprint = Some((h.to_string(), f.to_string()));
                    }
                }
                "rtpmap" => {
                    if let Some((id, rest)) = val.split_once(' ') {
                        let mut parts = rest.split('/');
                        let name = parts.next().unwrap_or_default().to_string();
                        let rate = parts.next().unwrap_or_default().to_string();
                        let ch = parts.next().unwrap_or("1").to_string();
                        p.payloads.push((id.to_string(), name, rate, ch));
                    }
                }
                "fmtp" => {
                    if let Some((id, rest)) = val.split_once(' ') {
                        let params = rest.split(';').filter_map(|kv| kv.trim().split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();
                        p.fmtp.insert(id.to_string(), params);
                    }
                }
                "rtcp-fb" => {
                    let mut it = val.split_whitespace();
                    if let (Some(id), Some(t)) = (it.next(), it.next()) {
                        p.rtcp_fb.entry(id.to_string()).or_default().push((t.to_string(), it.next().map(str::to_string)));
                    }
                }
                "extmap" => {
                    let mut it = val.split_whitespace();
                    if let (Some(id), Some(uri)) = (it.next(), it.next()) {
                        p.extmap.push((id.split('/').next().unwrap_or(id).to_string(), uri.to_string()));
                    }
                }
                "ssrc" => {
                    if let Some((id, rest)) = val.split_once(' ') {
                        let (k, v) = rest.split_once(':').unwrap_or((rest, ""));
                        p.ssrc.entry(id.to_string()).or_default().push((k.to_string(), v.to_string()));
                    }
                }
                "candidate" => p.candidates.push(format!("candidate:{val}")),
                _ => {}
            }
        }
    }
    p.payloads.sort_by_key(|(id, ..)| order.iter().position(|o| o == id).unwrap_or(usize::MAX));
    p
}

fn sdp_to_content(p: &ParsedSdp, name: &str) -> Element {
    let mut desc = Element::builder("description", NS_RTP).attr(nc("media"), "audio");
    for (id, pname, rate, ch) in &p.payloads {
        let mut pt = Element::builder("payload-type", NS_RTP)
            .attr(nc("id"), id.as_str())
            .attr(nc("name"), pname.as_str())
            .attr(nc("clockrate"), rate.as_str())
            .attr(nc("channels"), ch.as_str());
        for (k, v) in p.fmtp.get(id).into_iter().flatten() {
            pt = pt.append(Element::builder("parameter", NS_RTP).attr(nc("name"), k.as_str()).attr(nc("value"), v.as_str()));
        }
        for (t, sub) in p.rtcp_fb.get(id).into_iter().flatten() {
            let mut fb = Element::builder("rtcp-fb", NS_FB).attr(nc("type"), t.as_str());
            if let Some(s) = sub {
                fb = fb.attr(nc("subtype"), s.as_str());
            }
            pt = pt.append(fb);
        }
        desc = desc.append(pt);
    }
    for (id, uri) in &p.extmap {
        desc = desc.append(Element::builder("rtp-hdrext", NS_HDREXT).attr(nc("id"), id.as_str()).attr(nc("uri"), uri.as_str()));
    }
    for (ssrc, params) in &p.ssrc {
        let mut src = Element::builder("source", NS_SSMA).attr(nc("ssrc"), ssrc.as_str());
        for (k, v) in params {
            src = src.append(Element::builder("parameter", NS_SSMA).attr(nc("name"), k.as_str()).attr(nc("value"), v.as_str()));
        }
        desc = desc.append(src);
    }
    if p.rtcp_mux {
        desc = desc.append(Element::builder("rtcp-mux", NS_RTP));
    }
    let mut transport = Element::builder("transport", NS_ICE).attr(nc("ufrag"), p.ufrag.as_str()).attr(nc("pwd"), p.pwd.as_str());
    if let Some((hash, fp)) = &p.fingerprint {
        transport = transport.append(
            Element::builder("fingerprint", NS_DTLS)
                .attr(nc("hash"), hash.as_str())
                .attr(nc("setup"), p.setup.as_str())
                .append(fp.as_str()),
        );
    }
    for c in &p.candidates {
        if let Some(el) = candidate_to_jingle(c) {
            transport = transport.append(el);
        }
    }
    Element::builder("content", NS_JINGLE)
        .attr(nc("creator"), "initiator")
        .attr(nc("name"), name)
        .attr(nc("senders"), "both")
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

fn jingle_to_sdp(j: &Element) -> String {
    let mut s = String::from("v=0\r\no=- 1 2 IN IP4 0.0.0.0\r\ns=-\r\nt=0 0\r\n");
    let contents: Vec<&Element> = j
        .children()
        .filter(|c| c.is("content", NS_JINGLE))
        .filter(|c| c.get_child("description", NS_RTP).is_some_and(|d| d.attr("media") == Some("audio")))
        .collect();
    let names: Vec<&str> = contents.iter().filter_map(|c| c.attr("name")).collect();
    s += &format!("a=group:BUNDLE {}\r\na=msid-semantic: WMS *\r\n", names.join(" "));
    for content in contents {
        let desc = content.get_child("description", NS_RTP).unwrap();
        let pts: Vec<&Element> = desc.children().filter(|e| e.is("payload-type", NS_RTP)).collect();
        let ids: Vec<&str> = pts.iter().filter_map(|p| p.attr("id")).collect();
        s += &format!("m=audio 9 UDP/TLS/RTP/SAVPF {}\r\nc=IN IP4 0.0.0.0\r\na=rtcp:9 IN IP4 0.0.0.0\r\n", ids.join(" "));
        if let Some(t) = content.get_child("transport", NS_ICE) {
            s += &format!("a=ice-ufrag:{}\r\na=ice-pwd:{}\r\n", t.attr("ufrag").unwrap_or_default(), t.attr("pwd").unwrap_or_default());
            if let Some(fp) = t.get_child("fingerprint", NS_DTLS) {
                s += &format!("a=fingerprint:{} {}\r\na=setup:{}\r\n", fp.attr("hash").unwrap_or("sha-256"), fp.text().trim(), fp.attr("setup").unwrap_or("actpass"));
            }
            for c in t.children().filter(|e| e.is("candidate", NS_ICE)) {
                if let Some(line) = jingle_to_candidate(c) {
                    s += &format!("a={line}\r\n");
                }
            }
        }
        s += &format!("a=mid:{}\r\n", content.attr("name").unwrap_or("0"));
        let dir = match content.attr("senders").unwrap_or("both") {
            "initiator" | "responder" => "sendonly",
            "none" => "inactive",
            _ => "sendrecv",
        };
        s += &format!("a={dir}\r\n");
        if desc.get_child("rtcp-mux", NS_RTP).is_some() {
            s += "a=rtcp-mux\r\n";
        }
        for e in desc.children().filter(|e| e.is("rtp-hdrext", NS_HDREXT)) {
            s += &format!("a=extmap:{} {}\r\n", e.attr("id").unwrap_or_default(), e.attr("uri").unwrap_or_default());
        }
        for p in pts {
            let id = p.attr("id").unwrap_or_default();
            let ch = p.attr("channels").filter(|c| *c != "1").map(|c| format!("/{c}")).unwrap_or_default();
            s += &format!("a=rtpmap:{id} {}/{}{ch}\r\n", p.attr("name").unwrap_or_default(), p.attr("clockrate").unwrap_or("8000"));
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

    const OFFER: &str = "v=0\r\no=- 123 2 IN IP4 127.0.0.1\r\ns=-\r\nt=0 0\r\na=group:BUNDLE 0\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111 0\r\nc=IN IP4 0.0.0.0\r\na=ice-ufrag:uf\r\na=ice-pwd:pw\r\n\
a=fingerprint:sha-256 AA:BB\r\na=setup:actpass\r\na=mid:0\r\na=sendrecv\r\na=rtcp-mux\r\n\
a=rtpmap:111 opus/48000/2\r\na=fmtp:111 minptime=10;useinbandfec=1\r\na=rtcp-fb:111 transport-cc\r\n\
a=rtpmap:0 PCMU/8000\r\na=ssrc:42 cname:abc\r\n\
a=candidate:1 1 udp 2130706431 192.168.1.5 50000 typ host generation 0\r\n";

    #[test]
    fn sdp_jingle_roundtrip_keeps_what_webrtc_needs() {
        let p = parse_sdp(OFFER);
        assert_eq!((p.ufrag.as_str(), p.pwd.as_str(), p.setup.as_str()), ("uf", "pw", "actpass"));
        assert_eq!(p.payloads.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["111", "0"]);

        let content = sdp_to_content(&p, "0");
        let jingle = Element::builder("jingle", NS_JINGLE).attr(nc("action"), "session-initiate").append(content).build();
        let back = jingle_to_sdp(&jingle);
        for needle in [
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
        ] {
            assert!(back.contains(needle), "missing {needle:?} in\n{back}");
        }
    }

    #[test]
    fn candidate_with_related_address() {
        let el = candidate_to_jingle("candidate:2 1 UDP 1694498815 1.2.3.4 6000 typ srflx raddr 10.0.0.2 rport 5000").unwrap();
        assert_eq!(el.attr("type"), Some("srflx"));
        assert_eq!(el.attr("protocol"), Some("udp"));
        assert_eq!(jingle_to_candidate(&el).unwrap(), "candidate:2 1 udp 1694498815 1.2.3.4 6000 typ srflx raddr 10.0.0.2 rport 5000 generation 0");
    }
}
