//! Group calls in a MUC room, XEP-0272 (Muji): who takes part is announced in the room presence,
//! and every two participants hold a Jingle session of their own (a mesh, no media server).
//! Audio only: our voice is encoded once and the same Opus packets go to everyone; everyone is
//! played mixed. Sessions go between real JIDs, so the room must show them (ours do).
use crate::t;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures::{FutureExt as _, StreamExt};
use rtc::peer_connection::configuration::media_engine::MediaEngine;
use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodecParameters, RtpCodecKind};
use tokio_xmpp::IqRequest;
use tokio_xmpp::jid::{BareJid, Jid};
use tokio_xmpp::minidom::{Element, ElementBuilder};
use tokio_xmpp::parsers::stanza_error::DefinedCondition;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCConfigurationBuilder, RTCIceCandidateInit, RTCPeerConnectionIceEvent, RTCPeerConnectionState,
    RTCSessionDescription,
};

use crate::audio::{self, Audio};
use crate::call::{self, ACTIONS, JingleError, NS_GROUP, NS_ICE, NS_JINGLE, NS_RTP, NS_RTP_INFO, OPUS_PT};
use crate::xmpp::{Ev, Net, nc};

pub const NS_MUJI: &str = "urn:xmpp:jingle:muji:0";

/// How long we wait for others who were getting ready before us (a client may never finish).
const PREPARE_TIMEOUT: Duration = Duration::from_secs(5);

pub enum GroupCmd {
    /// Start the call in `room`, or join the one going on there.
    Join { room: String },
    Leave,
    Mute(bool),
    /// Nobody is played (our sound off).
    Deafen(bool),
    /// The chosen microphone or speaker changed.
    Devices,
}

#[derive(Debug)]
pub enum GroupEv {
    /// We are in the call of `room` (connecting to the others goes on).
    Joined { room: String },
    /// We are out of it; `reason` says why when it was not our choice.
    Ended { room: String, reason: String },
    /// A participant we hold a session with; `key` is its audio in `audio::take_source_levels`.
    Peer { nick: String, jid: String, key: u64, connected: bool, muted: bool },
    PeerGone { nick: String },
}

/// What an occupant's presence says about the call (`<muji>`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Muji {
    /// Getting ready to join or to change its streams: others wait before connecting.
    pub preparing: bool,
    /// Announces streams: it is in the call and expects a session from whoever joins.
    pub contents: bool,
}

pub fn muji_of(payloads: &[Element]) -> Option<Muji> {
    let m = payloads.iter().find(|e| e.is("muji", NS_MUJI))?;
    Some(Muji { preparing: m.children().any(|c| c.name() == "preparing"), contents: m.children().any(|c| c.name() == "content") })
}

fn muji_preparing() -> Element {
    Element::builder("muji", NS_MUJI).append(Element::builder("preparing", NS_MUJI)).build()
}

/// Our streams: Opus audio, under the payload type our offers use.
fn muji_audio() -> Element {
    let pt = Element::builder("payload-type", NS_RTP).attr(nc("id"), OPUS_PT.to_string()).attr(nc("name"), "opus").attr(nc("clockrate"), "48000").attr(nc("channels"), "2");
    let desc = Element::builder("description", NS_RTP).attr(nc("media"), "audio").append(pt);
    Element::builder("muji", NS_MUJI).append(Element::builder("content", NS_MUJI).attr(nc("creator"), "initiator").attr(nc("name"), "audio").append(desc)).build()
}

#[derive(Clone)]
struct Occupant {
    /// Real full JID: where Jingle goes.
    jid: Option<Jid>,
    muji: Option<Muji>,
}

enum Msg {
    /// `nick`: ours in the room, for `Join` (none when we are not in it).
    Cmd(GroupCmd, Option<String>),
    Occupant { room: BareJid, nick: String, jid: Option<Jid>, muji: Option<Muji>, gone: bool, me: bool },
    RoomGone(BareJid),
    Disconnected,
    Jingle(Jid, Element),
    PrepareTimeout(String),
    Candidate(String, RTCIceCandidateInit),
    PcState(String, RTCPeerConnectionState),
    Track(String, Arc<dyn TrackRemote>),
}

/// Front end owned by the XMPP loop; the call runs in its own task.
pub struct GroupCalls {
    tx: UnboundedSender<Msg>,
    /// Our sessions: Jingle requests for them come here, not to 1:1 calls.
    sids: Arc<Mutex<HashSet<String>>>,
    bound: Arc<Mutex<Option<Jid>>>,
}

impl GroupCalls {
    /// `busy` is set while we are in a call (1:1 calls are turned down meanwhile).
    pub fn new(net: Net, me: BareJid, busy: Arc<AtomicBool>) -> Self {
        let (tx, mut rx) = unbounded();
        let (sids, bound) = (Arc::default(), Arc::default());
        let mut actor = Actor { net, me, tx: tx.clone(), sids: Arc::clone(&sids), bound: Arc::clone(&bound), busy, rooms: HashMap::new(), call: None, next_key: 0 };
        tokio::spawn(async move {
            while let Some(msg) = rx.next().await {
                // A bug must not take later calls with it: drop this one, go on.
                if std::panic::AssertUnwindSafe(actor.handle(msg)).catch_unwind().await.is_err() {
                    actor.end(t!("call.reason_dropped"), true).await;
                }
            }
        });
        Self { tx, sids, bound }
    }

    fn send(&self, msg: Msg) {
        let _ = self.tx.unbounded_send(msg);
    }

    pub fn set_bound(&self, jid: Jid) {
        *self.bound.lock().unwrap() = Some(jid);
    }

    pub fn on_cmd(&self, cmd: GroupCmd, nick: Option<String>) {
        self.send(Msg::Cmd(cmd, nick));
    }

    /// Every occupant presence of every room: who is in which call, and their real JIDs.
    pub fn on_occupant(&self, room: BareJid, nick: String, jid: Option<Jid>, muji: Option<Muji>, gone: bool, me: bool) {
        self.send(Msg::Occupant { room, nick, jid, muji, gone, me });
    }

    /// We left `room` or were removed from it.
    pub fn on_room_gone(&self, room: BareJid) {
        self.send(Msg::RoomGone(room));
    }

    pub fn on_disconnected(&self) {
        self.send(Msg::Disconnected);
    }

    /// Whether a Jingle request belongs to a group call.
    pub fn owns(&self, el: &Element) -> bool {
        el.has_child("muji", NS_MUJI) || el.attr("sid").is_some_and(|s| self.sids.lock().unwrap().contains(s))
    }

    /// Checked here so the IQ can be answered; the work happens in the actor.
    pub fn on_jingle(&self, from: Option<Jid>, el: Element) -> Result<(), JingleError> {
        let from = from.ok_or(JingleError::new(DefinedCondition::BadRequest, None))?;
        let action = el.attr("action").unwrap_or_default();
        if !ACTIONS.contains(&action) {
            return Err(JingleError::new(DefinedCondition::BadRequest, None));
        }
        let sid = el.attr("sid").unwrap_or_default().to_string();
        let mut sids = self.sids.lock().unwrap();
        if action == "session-initiate" {
            // Its transport-info may come before the actor has seen it: route those here already.
            sids.insert(sid);
        } else if !sids.contains(&sid) {
            return Err(JingleError::new(DefinedCondition::ItemNotFound, Some("unknown-session")));
        }
        drop(sids);
        self.send(Msg::Jingle(from, el));
        Ok(())
    }
}

/// One participant we hold a session with.
struct Peer {
    nick: String,
    jid: Jid,
    /// We sent the session-initiate.
    initiator: bool,
    /// Its audio in the mix.
    key: u64,
    pc: Option<Arc<dyn PeerConnection>>,
    /// Content names (SDP mids), and the ufrag our candidates go with.
    contents: Vec<String>,
    ufrag: String,
    pending_candidates: Vec<RTCIceCandidateInit>,
    remote_set: bool,
    connected: bool,
    muted: bool,
}

enum Stage {
    /// Our `<preparing/>` is out: we wait for its echo, then for whoever was preparing before us.
    Preparing { echoed: bool, waiting: HashSet<String> },
    Joined,
}

/// Each session's track and the Opus payload type it negotiated: where our audio goes.
type Senders = Arc<Mutex<Vec<(String, Arc<TrackLocalStaticRTP>, u8)>>>;

struct Call {
    id: String,
    room: BareJid,
    nick: String,
    stage: Stage,
    audio: Arc<Audio>,
    senders: Senders,
    ssrc: u32,
    /// By session id.
    peers: HashMap<String, Peer>,
    muted: bool,
    /// Keeps the computer awake while the call is up.
    _awake: Option<crate::awake::Awake>,
}

struct Actor {
    net: Net,
    me: BareJid,
    tx: UnboundedSender<Msg>,
    sids: Arc<Mutex<HashSet<String>>>,
    bound: Arc<Mutex<Option<Jid>>>,
    busy: Arc<AtomicBool>,
    /// Occupants of the rooms we are in, as their presence describes them.
    rooms: HashMap<BareJid, HashMap<String, Occupant>>,
    call: Option<Call>,
    next_key: u64,
}

/// Encodes 20 ms frames once and sends them to every session.
async fn write_audio(mut frames: UnboundedReceiver<Vec<f32>>, senders: Senders, ssrc: u32) {
    let (mut seq, mut ts): (u16, u32) = (rand::random(), rand::random());
    let mut opus = audio::OpusEncoder::new().ok();
    while let Some(pcm) = frames.next().await {
        let Some(payload) = opus.as_mut().and_then(|e| e.encode(&pcm)) else { continue };
        let payload = bytes::Bytes::from(payload);
        let targets = senders.lock().unwrap().clone();
        for (_, track, pt) in targets {
            let header = rtc::rtp::header::Header { version: 2, payload_type: pt, sequence_number: seq, timestamp: ts, ssrc, ..Default::default() };
            // Errors before a transport is up are expected; the frame is just dropped for it.
            let _ = track.write_rtp(rtc::rtp::Packet { header, payload: payload.clone() }).await;
        }
        seq = seq.wrapping_add(1);
        ts = ts.wrapping_add(audio::CALL_FRAME as u32);
    }
}

/// Opus only: Muji wants one encoding that every participant takes.
fn media_engine() -> Result<MediaEngine, String> {
    let mut media = MediaEngine::default();
    media.register_codec(RTCRtpCodecParameters { rtp_codec: call::opus(), payload_type: OPUS_PT }, RtpCodecKind::Audio).map_err(|e| e.to_string())?;
    Ok(media)
}

/// Candidates of a transport-info (or of the transports of an offer/answer) as webrtc wants them.
fn candidates(el: &Element) -> Vec<RTCIceCandidateInit> {
    let mut out = vec![];
    for content in el.children().filter(|e| e.is("content", NS_JINGLE)) {
        let mid = content.attr("name").unwrap_or("0").to_string();
        for t in content.children().filter(|e| e.is("transport", NS_ICE)) {
            for cand in t.children().filter(|e| e.is("candidate", NS_ICE)) {
                if let Some(s) = call::jingle_to_candidate(cand) {
                    out.push(RTCIceCandidateInit { candidate: s, sdp_mid: Some(mid.clone()), sdp_mline_index: Some(0), username_fragment: t.attr("ufrag").map(str::to_string), ..Default::default() });
                }
            }
        }
    }
    out
}

impl Actor {
    async fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Cmd(cmd, nick) => self.on_cmd(cmd, nick).await,
            Msg::Occupant { room, nick, jid, muji, gone, me } => self.on_occupant(room, nick, jid, muji, gone, me).await,
            Msg::RoomGone(room) => {
                self.rooms.remove(&room);
                if self.call.as_ref().is_some_and(|c| c.room == room) {
                    self.end(t!("gcall.reason_room_left"), true).await;
                }
            }
            Msg::Disconnected => {
                self.rooms.clear();
                self.end(t!("call.reason_dropped"), false).await;
            }
            Msg::Jingle(from, el) => self.on_jingle(from, el).await,
            Msg::PrepareTimeout(id) => {
                if let Some(c) = self.call.as_mut().filter(|c| c.id == id)
                    && let Stage::Preparing { echoed, waiting } = &mut c.stage
                {
                    *echoed = true;
                    waiting.clear();
                }
                self.check_ready().await;
            }
            Msg::Candidate(sid, cand) => {
                let Some(p) = self.peer(&sid) else { return };
                let Some(el) = call::candidate_to_jingle(&cand.candidate) else { return };
                let transport = Element::builder("transport", NS_ICE).attr(nc("ufrag"), p.ufrag.as_str()).append(el);
                let name = p.contents.first().map_or("0", String::as_str);
                let content = Element::builder("content", NS_JINGLE).attr(nc("creator"), "initiator").attr(nc("name"), name).append(transport);
                let to = p.jid.clone();
                self.iq(to, self.jingle("transport-info", &sid).append(content).build());
            }
            Msg::PcState(sid, state) => match state {
                RTCPeerConnectionState::Connected => {
                    if let Some(p) = self.peer_mut(&sid) {
                        p.connected = true;
                    }
                    self.emit_peer(&sid);
                }
                RTCPeerConnectionState::Failed => self.drop_peer(&sid, Some("failed-transport")).await,
                _ => {}
            },
            Msg::Track(sid, track) => {
                let (Some(c), Some(p)) = (self.call.as_ref(), self.peer(&sid)) else { return };
                // Weak: a track that outlives the call must not keep the devices open.
                let (audio, key) = (Arc::downgrade(&c.audio), p.key);
                tokio::spawn(receive_audio(track, audio, key));
            }
        }
    }

    async fn on_cmd(&mut self, cmd: GroupCmd, nick: Option<String>) {
        match cmd {
            GroupCmd::Join { room } => {
                if self.call.is_some() {
                    return;
                }
                let Ok(room) = BareJid::from_str(&room) else { return };
                match nick {
                    Some(nick) => self.join(room, nick),
                    None => self.net.emit(Ev::Group(GroupEv::Ended { room: room.to_string(), reason: t!("gcall.reason_not_in_room").into() })),
                }
            }
            GroupCmd::Leave => self.end("", true).await,
            GroupCmd::Mute(m) => {
                let Some(c) = self.call.as_mut() else { return };
                c.muted = m;
                c.audio.muted.store(m, Ordering::Relaxed);
                let sids: Vec<String> = c.peers.iter().filter(|(_, p)| p.remote_set).map(|(s, _)| s.clone()).collect();
                for sid in sids {
                    self.mute_info(&sid, m);
                }
            }
            GroupCmd::Deafen(d) => {
                if let Some(c) = self.call.as_ref() {
                    c.audio.deafened.store(d, Ordering::Relaxed);
                }
            }
            GroupCmd::Devices => {
                let Some(audio) = self.call.as_ref().map(|c| c.audio.clone()) else { return };
                // A device can take a moment to open; keep the actor free meanwhile.
                if let Ok(Err(e)) = tokio::task::spawn_blocking(move || audio.restart()).await {
                    self.net.emit(Ev::Call(call::CallEv::Notice(t!("call.reason_error", error = e).to_string())));
                }
            }
        }
    }

    /// Opens the devices and announces we are getting ready (XEP-0272 §3).
    fn join(&mut self, room: BareJid, nick: String) {
        let (frames_tx, frames_rx) = unbounded::<Vec<f32>>();
        let audio = match audio::start(move |f| {
            let _ = frames_tx.unbounded_send(f);
        }) {
            Ok(a) => Arc::new(a),
            Err(e) => {
                let reason = t!("call.reason_error", error = e);
                return self.net.emit(Ev::Group(GroupEv::Ended { room: room.to_string(), reason }));
            }
        };
        let (senders, ssrc) = (Senders::default(), rand::random());
        tokio::spawn(write_audio(frames_rx, senders.clone(), ssrc));
        let id = crate::xmpp::new_id();
        self.busy.store(true, Ordering::Relaxed);
        self.call = Some(Call {
            id: id.clone(),
            room: room.clone(),
            nick,
            stage: Stage::Preparing { echoed: false, waiting: HashSet::new() },
            audio,
            senders,
            ssrc,
            peers: HashMap::new(),
            muted: false,
            _awake: crate::awake::keep_awake(t!("call.awake_reason")),
        });
        self.net.emit(Ev::Group(GroupEv::Joined { room: room.to_string() }));
        self.net.muji(room, Some(muji_preparing()));
        let tx = self.tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(PREPARE_TIMEOUT).await;
            let _ = tx.unbounded_send(Msg::PrepareTimeout(id));
        });
    }

    async fn on_occupant(&mut self, room: BareJid, nick: String, jid: Option<Jid>, muji: Option<Muji>, gone: bool, me: bool) {
        let occupants = self.rooms.entry(room.clone()).or_default();
        if gone {
            occupants.remove(&nick);
        } else {
            occupants.insert(nick.clone(), Occupant { jid, muji });
        }
        let Some(c) = self.call.as_mut().filter(|c| c.room == room) else { return };
        let preparing = muji.is_some_and(|m| m.preparing);
        let mut left = vec![];
        if me || nick == c.nick {
            // Our own <preparing/> came back: from now on we wait for those who were preparing then.
            if let Stage::Preparing { echoed, waiting } = &mut c.stage
                && !*echoed
                && preparing
            {
                *echoed = true;
                *waiting = occupants.iter().filter(|(n, o)| **n != c.nick && o.muji.is_some_and(|m| m.preparing)).map(|(n, _)| n.clone()).collect();
            }
        } else {
            if let Stage::Preparing { echoed: true, waiting } = &mut c.stage
                && !preparing
            {
                waiting.remove(&nick);
            }
            // Out of the call (or of the room): its session goes.
            if gone || muji.is_none() {
                left = c.peers.iter().filter(|(_, p)| p.nick == nick).map(|(s, _)| s.clone()).collect();
            }
        }
        for sid in left {
            self.drop_peer(&sid, Some("success")).await;
        }
        self.check_ready().await;
    }

    /// Everyone who was preparing before us is done: announce our streams and start a session
    /// with each participant already in the call (the joining side initiates, §3).
    async fn check_ready(&mut self) {
        let Some(c) = self.call.as_mut() else { return };
        let Stage::Preparing { echoed: true, waiting } = &c.stage else { return };
        if !waiting.is_empty() {
            return;
        }
        c.stage = Stage::Joined;
        let (room, nick) = (c.room.clone(), c.nick.clone());
        self.net.muji(room.clone(), Some(muji_audio()));
        let targets: Vec<(String, Jid)> = self
            .rooms
            .get(&room)
            .into_iter()
            .flatten()
            .filter(|(n, o)| **n != nick && o.muji.is_some_and(|m| m.contents))
            .filter_map(|(n, o)| Some((n.clone(), o.jid.clone()?)))
            .collect();
        for (nick, jid) in targets {
            if self.peer_by_nick(&nick).is_none() {
                self.initiate(nick, jid).await;
            }
        }
    }

    fn add_peer(&mut self, sid: &str, nick: String, jid: Jid, initiator: bool) {
        self.sids.lock().unwrap().insert(sid.to_string());
        let key = self.next_key;
        self.next_key += 1;
        let Some(c) = self.call.as_mut() else { return };
        let peer = Peer { nick, jid, initiator, key, pc: None, contents: vec![], ufrag: String::new(), pending_candidates: vec![], remote_set: false, connected: false, muted: false };
        c.peers.insert(sid.to_string(), peer);
    }

    fn peer(&self, sid: &str) -> Option<&Peer> {
        self.call.as_ref()?.peers.get(sid)
    }

    fn peer_mut(&mut self, sid: &str) -> Option<&mut Peer> {
        self.call.as_mut()?.peers.get_mut(sid)
    }

    /// Our session with `nick`: its id, and whether it is ours and not answered yet.
    fn peer_by_nick(&self, nick: &str) -> Option<(String, bool)> {
        self.call.as_ref()?.peers.iter().find(|(_, p)| p.nick == nick).map(|(s, p)| (s.clone(), p.initiator && !p.remote_set))
    }

    /// A peer connection for session `sid`, sending our audio track.
    async fn build_pc(&mut self, sid: &str) -> Result<Arc<dyn PeerConnection>, String> {
        let ice_servers = call::ice_servers(&self.net, self.me.domain().as_str()).await;
        let mut media = media_engine()?;
        let registry = call::interceptors(&mut media).map_err(|e| e.to_string())?;
        let handler = Arc::new(Handler { sid: sid.to_string(), tx: self.tx.clone() });
        let pc = PeerConnectionBuilder::new()
            .with_configuration(RTCConfigurationBuilder::new().with_ice_servers(ice_servers).build())
            .with_media_engine(media)
            .with_interceptor_registry(registry)
            .with_handler(handler)
            .with_udp_addrs(vec![format!("{}:0", call::local_ip())])
            .build()
            .await
            .map_err(|e| e.to_string())?;
        let pc: Arc<dyn PeerConnection> = Arc::new(pc);
        let (ssrc, senders) = self.call.as_ref().map(|c| (c.ssrc, c.senders.clone())).ok_or(t!("call.err_no_call"))?;
        let track = Arc::new(call::audio_track(sid, ssrc));
        pc.add_track(track.clone() as Arc<dyn TrackLocal>).await.map_err(|e| e.to_string())?;
        senders.lock().unwrap().push((sid.to_string(), track, OPUS_PT));
        let p = self.peer_mut(sid).ok_or(t!("call.err_no_call"))?;
        p.pc = Some(pc.clone());
        Ok(pc)
    }

    async fn initiate(&mut self, nick: String, jid: Jid) {
        let sid = crate::xmpp::new_id();
        self.add_peer(&sid, nick, jid, true);
        let res = async {
            let pc = self.build_pc(&sid).await?;
            let offer = pc.create_offer(None).await.map_err(|e| e.to_string())?;
            pc.set_local_description(offer).await.map_err(|e| e.to_string())?;
            Ok::<_, String>(pc.local_description().await.ok_or(t!("call.err_no_sdp").to_string())?.sdp)
        }
        .await;
        match res {
            Ok(sdp) => {
                self.send_contents("session-initiate", &sid, &sdp);
                self.emit_peer(&sid);
            }
            Err(e) => {
                eprintln!("group call: {e}");
                self.drop_peer(&sid, None).await;
            }
        }
    }

    async fn on_jingle(&mut self, from: Jid, el: Element) {
        let sid = el.attr("sid").unwrap_or_default().to_string();
        match el.attr("action").unwrap_or_default() {
            "session-initiate" => self.on_initiate(from, sid, el).await,
            "session-accept" => {
                let Some(pc) = self.peer(&sid).filter(|p| p.initiator && !p.remote_set).and_then(|p| p.pc.clone()) else { return };
                let sdp = call::contents_to_sdp(&call::rtp_contents(&el), "responder");
                let ok = match RTCSessionDescription::answer(sdp.clone()) {
                    Ok(desc) => pc.set_remote_description(desc).await.is_ok(),
                    Err(_) => false,
                };
                if !ok {
                    return self.drop_peer(&sid, Some("failed-application")).await;
                }
                self.set_format(&sid, &sdp);
                self.flush_candidates(&sid).await;
                if self.call.as_ref().is_some_and(|c| c.muted) {
                    self.mute_info(&sid, true);
                }
            }
            "transport-info" => {
                let cands = candidates(&el);
                let Some(p) = self.peer_mut(&sid) else { return };
                p.pending_candidates.extend(cands);
                if p.remote_set {
                    self.flush_candidates(&sid).await;
                }
            }
            "session-info" => {
                let Some(p) = self.peer_mut(&sid) else { return };
                for info in el.children().filter(|e| e.ns() == NS_RTP_INFO) {
                    match info.name() {
                        "mute" => p.muted = true,
                        "unmute" | "active" => p.muted = false,
                        _ => {}
                    }
                }
                self.emit_peer(&sid);
            }
            "session-terminate" => self.drop_peer(&sid, None).await,
            // Audio only for now: video from another client is declined, the session stays.
            "content-add" => {
                let mut reject = self.jingle("content-reject", &sid);
                for c in el.children().filter(|e| e.is("content", NS_JINGLE)) {
                    reject = reject.append(Element::builder("content", NS_JINGLE).attr(nc("creator"), c.attr("creator").unwrap_or("initiator")).attr(nc("name"), c.attr("name").unwrap_or_default()));
                }
                self.iq(from, reject.build());
            }
            _ => {}
        }
    }

    /// A participant who joined after us starts a session with us: answered right away, if it
    /// is in the room it names and we are in that call.
    async fn on_initiate(&mut self, from: Jid, sid: String, el: Element) {
        let room = el.get_child("muji", NS_MUJI).and_then(|m| m.attr("room")).and_then(|r| BareJid::from_str(r).ok());
        let in_call = self.call.as_ref().is_some_and(|c| Some(&c.room) == room.as_ref());
        let nick = room.as_ref().and_then(|r| self.rooms.get(r)).and_then(|occ| occ.iter().find(|(_, o)| o.jid.as_ref() == Some(&from)).map(|(n, _)| n.clone()));
        let Some(nick) = nick.filter(|_| in_call) else {
            self.sids.lock().unwrap().remove(&sid);
            return self.iq(from, self.jingle("session-terminate", &sid).append(call::reason("decline")).build());
        };
        if let Some((old, ours_pending)) = self.peer_by_nick(&nick) {
            // Both started one at once: the lower session id stays (as XEP-0166 §7.2.16 breaks ties).
            if ours_pending && old.as_bytes() < sid.as_bytes() {
                self.sids.lock().unwrap().remove(&sid);
                return self.iq(from, self.jingle("session-terminate", &sid).append(call::reason("alternative-session")).build());
            }
            // Theirs wins, or they came back (a reconnect): the old session goes.
            self.drop_peer(&old, (!ours_pending).then_some("success")).await;
        }
        self.add_peer(&sid, nick, from, false);
        let remote = call::rtp_contents(&el);
        let res = async {
            let pc = self.build_pc(&sid).await?;
            let desc = RTCSessionDescription::offer(call::contents_to_sdp(&remote, "initiator")).map_err(|e| e.to_string())?;
            pc.set_remote_description(desc).await.map_err(|e| e.to_string())?;
            let answer = pc.create_answer(None).await.map_err(|e| e.to_string())?;
            pc.set_local_description(answer).await.map_err(|e| e.to_string())?;
            Ok::<_, String>(pc.local_description().await.ok_or(t!("call.err_no_sdp").to_string())?.sdp)
        }
        .await;
        match res {
            Ok(sdp) => {
                self.set_format(&sid, &sdp);
                self.flush_candidates(&sid).await;
                self.send_contents("session-accept", &sid, &sdp);
                if self.call.as_ref().is_some_and(|c| c.muted) {
                    self.mute_info(&sid, true);
                }
                self.emit_peer(&sid);
            }
            Err(e) => {
                eprintln!("group call: {e}");
                self.drop_peer(&sid, Some("failed-application")).await;
            }
        }
    }

    /// The Opus payload type the session settled on (answers echo the offer's).
    fn set_format(&self, sid: &str, sdp: &str) {
        let (Some(c), Some(f)) = (self.call.as_ref(), call::formats(sdp).0) else { return };
        if let Some(s) = c.senders.lock().unwrap().iter_mut().find(|s| s.0 == sid) {
            s.2 = f.pt;
        }
    }

    async fn flush_candidates(&mut self, sid: &str) {
        let Some(p) = self.peer_mut(sid) else { return };
        p.remote_set = true;
        let (pc, cands) = (p.pc.clone(), std::mem::take(&mut p.pending_candidates));
        if let Some(pc) = pc {
            for cand in cands {
                let _ = pc.add_ice_candidate(cand).await;
            }
        }
    }

    /// Our SDP as Jingle contents; a session-initiate names the room (§3).
    fn send_contents(&mut self, action: &str, sid: &str, sdp: &str) {
        let room = self.call.as_ref().map(|c| c.room.to_string());
        let Some(p) = self.peer_mut(sid) else { return };
        let parsed = call::parse_sdp(sdp);
        p.contents = parsed.sections.iter().map(|s| s.mid.clone()).collect();
        p.ufrag = parsed.ufrag.clone();
        let (initiator, to, names) = (p.initiator, p.jid.clone(), p.contents.clone());
        let mut el = self.jingle(action, sid);
        if action == "session-initiate"
            && let Some(room) = room
        {
            el = el.append(Element::builder("muji", NS_MUJI).attr(nc("room"), room));
        }
        for (i, s) in parsed.sections.iter().enumerate() {
            el = el.append(call::sdp_to_content(&parsed, s, initiator, i == 0));
        }
        let mut group = Element::builder("group", NS_GROUP).attr(nc("semantics"), "BUNDLE");
        for name in &names {
            group = group.append(Element::builder("content", NS_GROUP).attr(nc("name"), name.as_str()));
        }
        self.iq(to, el.append(group).build());
    }

    fn mute_info(&self, sid: &str, mute: bool) {
        let Some(p) = self.peer(sid) else { return };
        let name = p.contents.first().map_or("0", String::as_str);
        let info = Element::builder(if mute { "mute" } else { "unmute" }, NS_RTP_INFO).attr(nc("creator"), "initiator").attr(nc("name"), name);
        self.iq(p.jid.clone(), self.jingle("session-info", sid).append(info).build());
    }

    /// Closes one session; `why`: also tell the participant (not when they ended it).
    async fn drop_peer(&mut self, sid: &str, why: Option<&str>) {
        self.sids.lock().unwrap().remove(sid);
        let Some(c) = self.call.as_mut() else { return };
        let Some(p) = c.peers.remove(sid) else { return };
        c.senders.lock().unwrap().retain(|s| s.0 != sid);
        c.audio.remove_source(p.key);
        if let Some(why) = why {
            self.iq(p.jid.clone(), self.jingle("session-terminate", sid).append(call::reason(why)).build());
        }
        if let Some(pc) = p.pc {
            let _ = pc.close().await;
        }
        // A new session with the same participant may already stand in for this one.
        if self.peer_by_nick(&p.nick).is_none() {
            self.net.emit(Ev::Group(GroupEv::PeerGone { nick: p.nick }));
        }
    }

    /// Leaves the call; `signal`: take `<muji>` out of our presence and close every session
    /// (not when the connection is gone anyway).
    async fn end(&mut self, reason: &str, signal: bool) {
        let Some(c) = self.call.take() else { return };
        self.busy.store(false, Ordering::Relaxed);
        {
            let mut sids = self.sids.lock().unwrap();
            for sid in c.peers.keys() {
                sids.remove(sid);
            }
        }
        if signal {
            self.net.muji(c.room.clone(), None);
            for (sid, p) in &c.peers {
                self.iq(p.jid.clone(), self.jingle("session-terminate", sid).append(call::reason("success")).build());
            }
        }
        for p in c.peers.into_values() {
            if let Some(pc) = p.pc {
                let _ = pc.close().await;
            }
        }
        self.net.emit(Ev::Group(GroupEv::Ended { room: c.room.to_string(), reason: reason.to_string() }));
    }

    fn emit_peer(&self, sid: &str) {
        let Some(p) = self.peer(sid) else { return };
        self.net.emit(Ev::Group(GroupEv::Peer { nick: p.nick.clone(), jid: p.jid.to_bare().to_string(), key: p.key, connected: p.connected, muted: p.muted }));
    }

    fn jingle(&self, action: &str, sid: &str) -> ElementBuilder {
        let me = self.bound.lock().unwrap().clone().map(|j| j.to_string()).unwrap_or_else(|| self.me.to_string());
        let b = Element::builder("jingle", NS_JINGLE).attr(nc("action"), action).attr(nc("sid"), sid);
        match action {
            "session-initiate" => b.attr(nc("initiator"), me),
            "session-accept" => b.attr(nc("responder"), me),
            _ => b,
        }
    }

    fn iq(&self, to: Jid, el: Element) {
        let net = self.net.clone();
        tokio::spawn(async move {
            let _ = net.iq(Some(to), IqRequest::Set(el)).await;
        });
    }
}

/// A participant's audio into the mix, under its key.
async fn receive_audio(track: Arc<dyn TrackRemote>, audio: Weak<Audio>, key: u64) {
    if track.kind().await != RtpCodecKind::Audio {
        return;
    }
    while let Some(ev) = track.poll().await {
        let TrackRemoteEvent::OnRtpPacket(p) = ev else { continue };
        let Some(audio) = audio.upgrade() else { return };
        call::RTP_RECEIVED.fetch_add(1, Ordering::Relaxed);
        audio.receive_from(key, audio::RtpAudio { seq: p.header.sequence_number, ts: p.header.timestamp, ssrc: p.header.ssrc, pcmu: p.header.payload_type == 0, payload: p.payload.to_vec() });
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn muji_presence() {
        let parse = |el: Element| muji_of(&[el]);
        assert_eq!(parse(muji_preparing()), Some(Muji { preparing: true, contents: false }));
        assert_eq!(parse(muji_audio()), Some(Muji { preparing: false, contents: true }));
        assert_eq!(muji_of(&[]), None);
        // As other clients write it (XEP-0272 example 3): contents, plus preparing while adding one.
        let theirs: Element = "<muji xmlns='urn:xmpp:jingle:muji:0'><content creator='initiator' name='voice'><description xmlns='urn:xmpp:jingle:apps:rtp:1' media='audio'/></content><preparing/></muji>".parse().unwrap();
        assert_eq!(parse(theirs), Some(Muji { preparing: true, contents: true }));
        let pt = muji_audio().get_child("content", NS_MUJI).and_then(|c| c.get_child("description", NS_RTP)).and_then(|d| d.get_child("payload-type", NS_RTP)).cloned().unwrap();
        assert_eq!((pt.attr("name"), pt.attr("id"), pt.attr("clockrate")), (Some("opus"), Some("111"), Some("48000")));
    }

    /// Jingle requests of group sessions are told apart from 1:1 calls: a session-initiate by its
    /// <muji>, the rest by the session ids we know.
    #[tokio::test]
    async fn jingle_routing() {
        let g = GroupCalls::new(Net::for_tests(), BareJid::from_str("me@x").unwrap(), Arc::default());
        let initiate: Element = "<jingle xmlns='urn:xmpp:jingle:1' action='session-initiate' sid='s1'><muji xmlns='urn:xmpp:jingle:muji:0' room='r@muc.x'/></jingle>".parse().unwrap();
        let info: Element = "<jingle xmlns='urn:xmpp:jingle:1' action='transport-info' sid='s1'/>".parse().unwrap();
        let other: Element = "<jingle xmlns='urn:xmpp:jingle:1' action='transport-info' sid='s2'/>".parse().unwrap();
        assert!(g.owns(&initiate) && !g.owns(&info));
        let from = Some(Jid::from_str("a@x/r").unwrap());
        assert!(g.on_jingle(from.clone(), initiate).is_ok());
        assert!(g.owns(&info) && !g.owns(&other), "its later requests come here too");
        assert_eq!(g.on_jingle(from, other).unwrap_err().jingle, Some("unknown-session"));
    }
}
