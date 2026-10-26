//! XMPP side: runs tokio-xmpp on its own thread and talks to the UI through channels.
//!
//! One loop owns the `Client`. Background tasks (uploads, MAM paging, vCards) never touch it
//! directly; they go through `Net`, which queues stanzas and IQs back into that loop.
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc::{UnboundedReceiver, UnboundedSender, unbounded};
use futures::channel::oneshot;
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::{
    bookmarks2::Conference,
    caps::{self, Caps},
    carbons,
    chatstates::ChatState,
    delay::Delay,
    disco::{DiscoInfoQuery, DiscoInfoResult, DiscoItemsQuery, DiscoItemsResult, Identity},
    displayed_markers::{Displayed, Markable},
    hashes::Algo,
    http_upload::{Header, SlotRequest, SlotResult},
    iq::Iq,
    jid::{BareJid, Jid},
    mam::{self, QueryId},
    message::{Id, Lang, Message, MessageType},
    message_correct::Replace,
    muc::{
        Muc,
        muc::History,
        user::{MucUser, Status as MucStatus},
    },
    ns,
    oob::Oob,
    ping::Ping,
    presence::{Presence, Show, Type as PresenceType},
    reactions::{Reaction, Reactions},
    receipts::{Received, Request as ReceiptRequest},
    roster::{Item as RosterItem, Roster, Subscription},
    rsm::SetQuery,
    stanza_error::{DefinedCondition, ErrorType, StanzaError},
    stanza_id::{OriginId, StanzaId},
    vcard::{Binval, Photo, Type as PhotoType, VCard, VCardQuery},
};
use tokio_xmpp::xmlstream::Timeouts;
use tokio_xmpp::{Client, Event, IqRequest, IqResponse, IqResponseToken, Stanza};

use crate::call::{CallCmd, CallEv, Calls};
use crate::e2e::{self, DeviceInfo};
use crate::omemo;
use crate::pgp;
use crate::connector::Connector;
use crate::store::{self, Msg};

pub const CAPS_NODE: &str = "https://github.com/auf-gg/crabber";

pub struct Account {
    pub jid: BareJid,
    pub password: String,
    pub host: Option<String>,
    pub port: u16,
    pub insecure: bool,
    pub dir: PathBuf,
}

#[derive(Clone, Debug)]
pub struct Contact {
    pub jid: String,
    pub name: String,
    pub subscription: String,
}

/// XEP-0402 room bookmark with autojoin set.
#[derive(Clone, Debug, PartialEq)]
pub struct Bookmark {
    pub room: String,
    pub name: String,
    pub nick: String,
}

pub enum Cmd {
    /// Sent by the UI after `Ev::Online`: MAM catch-up point and rooms to rejoin.
    Sync { mam_after: Option<String>, rooms: Vec<(String, String, Option<i64>)> },
    /// `enc`: chat encryption mode — "auto" | "omemo" | "pgp" | "plain".
    Send { to: String, id: String, body: String, groupchat: bool, enc: String },
    /// XEP-0308: new text for our message `replace` (its msg_id).
    Correct { to: String, replace: String, body: String, groupchat: bool, enc: String },
    /// XEP-0444: our full set of reactions to message `id` (MUC stanza-id in groups).
    React { to: String, id: String, emojis: Vec<String>, groupchat: bool },
    /// XEP-0402: store the room on the server so other devices join it too.
    Bookmark(Bookmark),
    Upload { to: String, id: String, path: PathBuf, groupchat: bool, enc: String },
    Download { url: String, path: PathBuf },
    Typing { to: String, on: bool },
    Displayed { to: String, id: String },
    AddContact { jid: String, name: String },
    RemoveContact { jid: String },
    Approve { jid: String },
    Deny { jid: String },
    Join { room: String, nick: String, since: Option<i64>, create_name: Option<String> },
    Leave { room: String },
    Invite { room: String, jid: String },
    SetShow(Option<Show>),
    SetAvatar(PathBuf),
    /// Asks for `Ev::E2eDevices` of a JID (our own JID lists our devices).
    E2eInfo { jid: String },
    E2eTrust { jid: String, device: u32, trusted: bool },
    E2eBlindTrust(bool),
    PgpGenerate,
    PgpImportSecret { armored: String },
    PgpDeleteSecret,
    PgpImportContact { jid: String, armored: String },
    /// Asks for `Ev::PgpOwn` and, with a JID, `Ev::PgpContact`.
    PgpInfo { jid: Option<String> },
    Call(CallCmd),
}

#[derive(Debug)]
pub enum Ev {
    Online { jid: String },
    Disconnected(String),
    Roster(Vec<Contact>),
    RosterItem(Contact),
    Presence { jid: String, online: bool },
    SubscribeRequest { jid: String },
    /// `live`: arrived just now (not from MAM, offline storage or MUC history) — worth a notification.
    /// `sid`: stanza-id the room assigned (groupchat only).
    Message { peer: String, groupchat: bool, msg: Msg, archive_id: Option<String>, live: bool, sid: Option<String> },
    /// XEP-0308: `sender` replaced the text of their message `id`.
    Correct { peer: String, id: String, sender: String, body: String },
    /// XEP-0444: `sender`'s full reaction set for message `id`.
    Reactions { peer: String, id: String, sender: String, emojis: Vec<String> },
    /// Server bookmarks at login; `None`: the account has no bookmarks node yet.
    Bookmarks(Option<Vec<Bookmark>>),
    BookmarkAdded(Bookmark),
    BookmarkRemoved { room: String },
    Receipt { peer: String, id: String },
    Displayed { peer: String, id: String, by_me: bool },
    Typing { peer: String, who: String, on: bool },
    RoomJoined { room: String, occupants: usize },
    RoomName { room: String, name: String },
    RoomError { room: String, error: String },
    Invite { room: String, from: String },
    Avatar { jid: String },
    Uploaded { peer: String, id: String, url: String },
    SendFailed { peer: String, id: String, error: String },
    Downloaded { url: String },
    /// Our outgoing message went out encrypted (1 = OMEMO, 2 = OpenPGP).
    SentEncrypted { peer: String, id: String, enc: u8 },
    E2eDevices { jid: String, devices: Vec<DeviceInfo> },
    PgpOwn { fingerprint: Option<String>, public: Option<String> },
    /// `hint`: key id from the contact's signed presence when we have no key yet.
    PgpContact { jid: String, fingerprint: Option<String>, hint: Option<String> },
    DownloadFailed { url: String, error: String },
    Error(String),
    Call(CallEv),
}

enum Out {
    Stanza(Box<Stanza>),
    /// A decrypted message fed back into normal processing.
    Inject(Box<Message>, Option<(String, Option<i64>)>, u8),
    Iq(Option<Jid>, IqRequest, oneshot::Sender<IqResponseToken>),
}

/// Cloneable handle that background tasks use to reach the client loop.
#[derive(Clone)]
pub struct Net {
    out: UnboundedSender<Out>,
    pub ev: UnboundedSender<Ev>,
}

impl Net {
    pub fn send(&self, s: impl Into<Stanza>) {
        let _ = self.out.unbounded_send(Out::Stanza(Box::new(s.into())));
    }
    pub fn inject(&self, m: Message, archived: Option<(String, Option<i64>)>, enc: u8) {
        let _ = self.out.unbounded_send(Out::Inject(Box::new(m), archived, enc));
    }
    pub fn emit(&self, e: Ev) {
        let _ = self.ev.unbounded_send(e);
    }
    pub async fn iq(&self, to: Option<Jid>, req: IqRequest) -> Result<Option<Element>, String> {
        let (tx, rx) = oneshot::channel();
        self.out.unbounded_send(Out::Iq(to, req, tx)).map_err(|_| "нет соединения".to_string())?;
        let token = rx.await.map_err(|_| "нет соединения".to_string())?;
        match tokio::time::timeout(Duration::from_secs(30), token).await {
            Ok(Ok(IqResponse::Result(el))) => Ok(el),
            Ok(Ok(IqResponse::Error(e))) => Err(format!("{:?}", e.defined_condition)),
            Ok(Err(e)) => Err(e.to_string()),
            Err(_) => Err("таймаут".into()),
        }
    }
}

/// Starts the connection. Dropping the returned sender disconnects.
pub fn connect(acc: Account) -> (UnboundedSender<Cmd>, UnboundedReceiver<Ev>) {
    let (cmd_tx, cmd_rx) = unbounded();
    let (ev_tx, ev_rx) = unbounded();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().unwrap();
        rt.block_on(run(acc, cmd_rx, ev_tx));
    });
    (cmd_tx, ev_rx)
}

struct Room {
    nick: String,
    occupants: HashSet<String>,
    joined: bool,
    create_name: Option<String>,
}

struct State {
    me: BareJid,
    dir: PathBuf,
    http: reqwest::Client,
    net: Net,
    rooms: HashMap<BareJid, Room>,
    resources: HashMap<BareJid, HashSet<String>>,
    upload: Arc<Mutex<Option<Jid>>>,
    show: Option<Show>,
    calls: Calls,
    e2e: Option<e2e::Shared>,
    /// Detached signature of our (empty) presence status, when we have an OpenPGP key.
    pgp_presence: Arc<Mutex<Option<String>>>,
    /// Contacts whose signed presence we already looked at this session.
    pgp_seen: HashSet<String>,
}

async fn run(acc: Account, mut cmds: UnboundedReceiver<Cmd>, ev: UnboundedSender<Ev>) {
    let connector = Connector { host: acc.host.clone(), port: acc.port, insecure: acc.insecure };
    let mut client = Client::new_with_connector(acc.jid.clone(), acc.password.clone(), connector, Timeouts::default());
    let (out_tx, mut out_rx) = unbounded();
    let net = Net { out: out_tx, ev };
    let http = reqwest::Client::builder()
        .tls_danger_accept_invalid_certs(acc.insecure)
        .build()
        .unwrap();
    let mut st = State {
        me: acc.jid.clone(),
        dir: acc.dir,
        http,
        net: net.clone(),
        rooms: HashMap::new(),
        resources: HashMap::new(),
        upload: Arc::new(Mutex::new(None)),
        show: None,
        calls: Calls::new(net.clone(), acc.jid.clone()),
        e2e: None,
        pgp_presence: Arc::new(Mutex::new(None)),
        pgp_seen: HashSet::new(),
    };
    match e2e::E2e::open(&st.dir, &acc.jid.to_string()) {
        Ok(e) => {
            *st.pgp_presence.lock().unwrap() = e.pgp_secret.as_ref().and_then(|k| pgp::sign_status("", k).ok());
            st.e2e = Some(std::sync::Arc::new(tokio::sync::Mutex::new(e)));
        }
        Err(e) => st.net.emit(Ev::Error(format!("OMEMO недоступен: {e}"))),
    }

    loop {
        tokio::select! {
            event = client.next() => match event {
                None => break,
                Some(Event::Online { bound_jid, .. }) => st.on_online(bound_jid),
                Some(Event::Disconnected(e)) => {
                    st.resources.clear();
                    for r in st.rooms.values_mut() { r.joined = false; r.occupants.clear(); }
                    st.net.emit(Ev::Disconnected(e.to_string()));
                }
                Some(Event::Stanza(stanza)) => st.on_stanza(stanza),
            },
            out = out_rx.next() => match out {
                // The stanza is queued before send_stanza waits for the write; don't let that wait stall the loop.
                Some(Out::Stanza(s)) => { let _ = tokio::time::timeout(Duration::from_secs(5), client.send_stanza(*s)).await; }
                Some(Out::Iq(to, req, reply)) => { let _ = reply.send(client.send_iq(to, req).await); }
                Some(Out::Inject(m, archived, enc)) => st.on_message(*m, archived, enc),
                None => break,
            },
            cmd = cmds.next() => match cmd {
                None => break, // UI dropped the sender: log out
                Some(cmd) => st.on_cmd(cmd),
            },
        }
    }
    let _ = client.send_end().await;
}

fn jid(s: &str) -> Option<BareJid> {
    BareJid::from_str(s).ok()
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn new_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
    format!("re{:x}{:x}", t, N.fetch_add(1, Ordering::Relaxed))
}

/// Features we advertise; calls need the Jingle RTP/ICE/DTLS set (XEP-0167/0176/0320).
fn disco_info(node: Option<String>) -> DiscoInfoResult {
    let features = [
        ns::DISCO_INFO,
        ns::CAPS,
        ns::RECEIPTS,
        ns::CHATSTATES,
        "urn:xmpp:chat-markers:0",
        ns::MUC,
        "jabber:x:conference",
        "jabber:x:oob",
        ns::PING,
        ns::JINGLE,
        ns::JINGLE_RTP,
        ns::JINGLE_RTP_AUDIO,
        ns::JINGLE_ICE_UDP,
        ns::JINGLE_DTLS,
        ns::JINGLE_MESSAGE,
        omemo::NS,
        "eu.siacs.conversations.axolotl.devicelist+notify",
        e2e::NS_EME,
        ns::MESSAGE_CORRECT,
        ns::REACTIONS,
        "urn:xmpp:bookmarks:1+notify",
    ];
    DiscoInfoResult {
        node,
        identities: vec![Identity::new("client", "pc", "en", "Crabber")],
        features: features.iter().map(|s| s.to_string()).collect(),
        extensions: vec![],
    }
}

fn caps() -> Caps {
    let hash = caps::hash_caps(caps_string(&disco_info(None)).as_bytes(), Algo::Sha_1).unwrap();
    Caps::new(CAPS_NODE, hash)
}

/// XEP-0115 verification string. Not `caps::compute_disco`: it sorts "x<" instead of "x", so
/// "…axolotl.devicelist+notify" lands before "…axolotl", the server rejects our hash and never
/// sends PEP notifications (OMEMO device lists then go stale).
fn caps_string(d: &DiscoInfoResult) -> String {
    let mut ids: Vec<String> = d
        .identities
        .iter()
        .map(|i| format!("{}/{}/{}/{}<", i.category, i.type_, i.lang.as_deref().unwrap_or(""), i.name.as_deref().unwrap_or("")))
        .collect();
    let mut features: Vec<&str> = d.features.iter().map(String::as_str).collect();
    ids.sort();
    features.sort();
    ids.concat() + &features.iter().map(|f| format!("{f}<")).collect::<String>()
}

fn msg_body(m: &Message) -> Option<String> {
    m.get_best_body_cloned(vec![]).map(|(_, b)| b)
}

impl State {
    fn presence(&self) -> Presence {
        let mut p = Presence::available().with_payload(caps());
        if let Some(sig) = self.pgp_presence.lock().unwrap().clone() {
            p.payloads.push(Element::builder("x", pgp::NS_SIGNED).append(sig).build());
        }
        p.show = self.show.clone();
        p
    }

    fn on_online(&mut self, bound: Jid) {
        self.calls.set_bound(bound.clone());
        self.net.emit(Ev::Online { jid: bound.to_string() });
        self.net.send(self.presence());
        let net = self.net.clone();
        let dir = self.dir.clone();
        let upload = self.upload.clone();
        let domain = self.me.domain().to_owned();
        tokio::spawn(async move {
            let _ = net.iq(None, IqRequest::Set(carbons::Enable.into())).await;
            if let Ok(Some(el)) = net.iq(None, IqRequest::Get(Roster { ver: None, items: vec![] }.into())).await
                && let Ok(roster) = Roster::try_from(el)
            {
                let contacts: Vec<Contact> = roster.items.iter().map(contact).collect();
                net.emit(Ev::Roster(contacts.clone()));
                for c in contacts {
                    fetch_avatar(&net, &dir, &c.jid).await;
                }
            }
            *upload.lock().unwrap() = discover_upload(&net, domain.as_str()).await;
        });
        let net = self.net.clone();
        let dir = self.dir.clone();
        let me = self.me.to_string();
        tokio::spawn(async move { fetch_avatar(&net, &dir, &me).await });
        let net = self.net.clone();
        tokio::spawn(async move {
            let req = Element::builder("pubsub", ns::PUBSUB).append(Element::builder("items", ns::PUBSUB).attr(nc("node"), ns::BOOKMARKS2)).build();
            match net.iq(None, IqRequest::Get(req)).await {
                Ok(el) => {
                    let list = el.as_ref().and_then(|e| e.get_child("items", ns::PUBSUB)).map(|i| i.children().filter_map(bookmark).collect()).unwrap_or_default();
                    net.emit(Ev::Bookmarks(Some(list)));
                }
                Err(e) if e == "ItemNotFound" => net.emit(Ev::Bookmarks(None)),
                // No PEP bookmarks on this server: rooms stay local-only.
                Err(e) => eprintln!("bookmarks: {e}"),
            }
        });
        if let Some(e2e) = self.e2e.clone() {
            tokio::spawn(e2e::on_online(e2e, self.net.clone()));
        }
    }

    fn on_cmd(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Sync { mam_after, rooms } => {
                let net = self.net.clone();
                tokio::spawn(mam_sync(net, mam_after));
                for (room, nick, since) in rooms {
                    self.join(room, nick, since, None);
                }
            }
            Cmd::Send { to, id, body, groupchat, enc } => self.send_text(to, id, body, groupchat, enc, None),
            // The correction gets its own stanza id; errors about it then match no stored message.
            Cmd::Correct { to, replace, body, groupchat, enc } => self.send_text(to, new_id(), body, groupchat, enc, Some(replace)),
            Cmd::React { to, id, emojis, groupchat } => {
                let mut m = Message::new_with_type(if groupchat { MessageType::Groupchat } else { MessageType::Chat }, jid(&to).map(Jid::from))
                    .with_payload(Reactions { id, reactions: emojis.into_iter().map(|emoji| Reaction { emoji }).collect() })
                    .with_payloads(vec![Element::builder("store", "urn:xmpp:hints").build()]);
                m.id = Some(Id(new_id()));
                self.net.send(m);
            }
            Cmd::Bookmark(b) => {
                let net = self.net.clone();
                tokio::spawn(async move {
                    let conf = Conference { autojoin: true, name: (!b.name.is_empty()).then_some(b.name), nick: b.nick.parse().ok(), password: None, extensions: None };
                    let field = |var: &str, value: &str| Element::builder("field", ns::DATA_FORMS).attr(nc("var"), var).append(Element::builder("value", ns::DATA_FORMS).append(value));
                    // XEP-0402 node options; whitelist keeps the room list private.
                    let options = Element::builder("x", ns::DATA_FORMS)
                        .attr(nc("type"), "submit")
                        .append(field("FORM_TYPE", "http://jabber.org/protocol/pubsub#publish-options"))
                        .append(field("pubsub#persist_items", "true"))
                        .append(field("pubsub#max_items", "max"))
                        .append(field("pubsub#send_last_published_item", "never"))
                        .append(field("pubsub#access_model", "whitelist"));
                    let item = Element::builder("item", ns::PUBSUB).attr(nc("id"), b.room).append(Element::from(conf));
                    let req = Element::builder("pubsub", ns::PUBSUB)
                        .append(Element::builder("publish", ns::PUBSUB).attr(nc("node"), ns::BOOKMARKS2).append(item))
                        .append(Element::builder("publish-options", ns::PUBSUB).append(options))
                        .build();
                    if let Err(e) = net.iq(None, IqRequest::Set(req)).await {
                        eprintln!("bookmark publish: {e}");
                    }
                });
            }
            Cmd::Upload { to, id, path, groupchat, enc } => {
                let (net, http, upload) = (self.net.clone(), self.http.clone(), self.upload.clone());
                let e2e = self.e2e.clone().filter(|_| !groupchat && enc != "plain");
                tokio::spawn(async move {
                    // Encrypted chats get an XEP-0454 aesgcm:// link sent inside OMEMO.
                    if let Some(e2e) = e2e {
                        let want = match encrypt_for(&e2e, &net, &to, &enc, "").await {
                            Ok(Some(_)) => true,
                            Ok(None) => false,
                            Err(error) => return net.emit(Ev::SendFailed { peer: to, id, error }),
                        };
                        if want {
                            let res = async {
                                let data = tokio::fs::read(&path).await.map_err(|e| e.to_string())?;
                                let (ct, fragment) = omemo::encrypt_file(&data);
                                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
                                let https = upload_bytes(&net, &http, upload, ct, name, "application/octet-stream").await?;
                                let url = format!("{}#{fragment}", https.replacen("https://", "aesgcm://", 1));
                                let el = e2e::encrypt(&e2e, &net, &to, &url).await?;
                                Ok::<_, String>((url, el))
                            }
                            .await;
                            match res {
                                Ok((url, el)) => {
                                    net.send(encrypted_message(&to, &id, el));
                                    net.emit(Ev::SentEncrypted { peer: to.clone(), id: id.clone(), enc: 1 });
                                    net.emit(Ev::Uploaded { peer: to, id, url });
                                }
                                Err(error) => net.emit(Ev::SendFailed { peer: to, id, error }),
                            }
                            return;
                        }
                    }
                    match upload_file(&net, &http, upload, &path).await {
                        Ok(url) => {
                            let mut m = Message::new_with_type(
                                if groupchat { MessageType::Groupchat } else { MessageType::Chat },
                                jid(&to).map(Jid::from),
                            )
                            .with_body(Lang::default(), url.clone())
                            .with_payload(Oob { url: url.clone(), desc: None });
                            m.id = Some(Id(id.clone()));
                            if !groupchat {
                                m = m.with_payload(ReceiptRequest).with_payload(Markable);
                            }
                            net.send(m);
                            net.emit(Ev::Uploaded { peer: to, id, url });
                        }
                        Err(error) => net.emit(Ev::SendFailed { peer: to, id, error }),
                    }
                });
            }
            Cmd::Download { url, path } => {
                let (net, http) = (self.net.clone(), self.http.clone());
                tokio::spawn(async move {
                    // Images are fetched automatically, so cap what a sender can make us download.
                    const MAX: u64 = 100 * 1024 * 1024;
                    let res = async {
                        // aesgcm:// (XEP-0454): fetch over https, decrypt with the key from the fragment.
                        let (fetch, fragment) = match url.strip_prefix("aesgcm://") {
                            Some(rest) => {
                                let (u, f) = rest.split_once('#').unwrap_or((rest, ""));
                                (format!("https://{u}"), Some(f.to_string()))
                            }
                            None => (url.clone(), None),
                        };
                        let resp = http.get(&fetch).send().await.map_err(|e| e.to_string())?.error_for_status().map_err(|e| e.to_string())?;
                        if resp.content_length().is_some_and(|n| n > MAX) {
                            return Err("файл больше 100 МБ".to_string());
                        }
                        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
                        if bytes.len() as u64 > MAX {
                            return Err("файл больше 100 МБ".to_string());
                        }
                        match fragment {
                            Some(f) => omemo::decrypt_file(&bytes, &f).map(bytes::Bytes::from),
                            None => Ok(bytes),
                        }
                    }
                    .await;
                    match res {
                        Ok(bytes) => {
                            let tmp = path.with_extension("part");
                            if tokio::fs::write(&tmp, &bytes).await.is_ok() && tokio::fs::rename(&tmp, &path).await.is_ok() {
                                net.emit(Ev::Downloaded { url });
                            }
                        }
                        Err(error) => net.emit(Ev::DownloadFailed { url, error }),
                    }
                });
            }
            Cmd::Typing { to, on } => {
                let Some(to) = jid(&to) else { return };
                let state = if on { ChatState::Composing } else { ChatState::Paused };
                self.net.send(Message::chat(Some(to.into())).with_payload(state));
            }
            Cmd::Displayed { to, id } => {
                let Some(to) = jid(&to) else { return };
                self.net.send(Message::chat(Some(to.into())).with_payload(Displayed { id: Id(id) }));
            }
            Cmd::AddContact { jid: j, name } => {
                let Some(to) = jid(&j) else { return };
                let item = RosterItem {
                    jid: to.clone(),
                    name: (!name.is_empty()).then_some(name),
                    subscription: Subscription::None,
                    ask: Default::default(),
                    groups: vec![],
                    approved: None,
                };
                let net = self.net.clone();
                tokio::spawn(async move {
                    if let Err(e) = net.iq(None, IqRequest::Set(Roster { ver: None, items: vec![item] }.into())).await {
                        net.emit(Ev::Error(format!("Не удалось добавить контакт: {e}")));
                    }
                    net.send(Presence::subscribe().with_to(to));
                });
            }
            Cmd::RemoveContact { jid: j } => {
                let Some(to) = jid(&j) else { return };
                let item = RosterItem {
                    jid: to,
                    name: None,
                    subscription: Subscription::Remove,
                    ask: Default::default(),
                    groups: vec![],
                    approved: None,
                };
                let net = self.net.clone();
                tokio::spawn(async move {
                    let _ = net.iq(None, IqRequest::Set(Roster { ver: None, items: vec![item] }.into())).await;
                });
            }
            Cmd::Approve { jid: j } => {
                let Some(to) = jid(&j) else { return };
                self.net.send(Presence::subscribed().with_to(to.clone()));
                self.net.send(Presence::subscribe().with_to(to));
            }
            Cmd::Deny { jid: j } => {
                if let Some(to) = jid(&j) {
                    self.net.send(Presence::new(PresenceType::Unsubscribed).with_to(to));
                }
            }
            Cmd::Join { room, nick, since, create_name } => self.join(room, nick, since, create_name),
            Cmd::Leave { room } => {
                let Some(r) = jid(&room) else { return };
                if let Some(state) = self.rooms.remove(&r)
                    && let Ok(full) = r.with_resource_str(&state.nick)
                {
                    self.net.send(Presence::unavailable().with_to(full));
                }
                let retract = Element::builder("pubsub", ns::PUBSUB)
                    .append(Element::builder("retract", ns::PUBSUB).attr(nc("node"), ns::BOOKMARKS2).attr(nc("notify"), "true").append(Element::builder("item", ns::PUBSUB).attr(nc("id"), room)))
                    .build();
                let net = self.net.clone();
                tokio::spawn(async move {
                    // item-not-found when it was never bookmarked: nothing to do.
                    let _ = net.iq(None, IqRequest::Set(retract)).await;
                });
            }
            Cmd::Invite { room, jid: j } => {
                let Some(r) = jid(&room) else { return };
                let x = Element::builder("x", ns::MUC_USER).append(Element::builder("invite", ns::MUC_USER).attr(nc("to"), j)).build();
                self.net.send(Message::new_with_type(MessageType::Normal, Some(r.into())).with_payloads(vec![x]));
            }
            Cmd::SetShow(show) => {
                self.show = show;
                self.net.send(self.presence());
                for (room, r) in &self.rooms {
                    if let Ok(full) = room.with_resource_str(&r.nick) {
                        self.net.send(self.presence().with_to(full));
                    }
                }
            }
            Cmd::SetAvatar(path) => {
                let (net, dir, me) = (self.net.clone(), self.dir.clone(), self.me.to_string());
                tokio::spawn(async move {
                    let Ok(data) = tokio::fs::read(&path).await else { return };
                    let vcard = VCard {
                        photo: Some(Photo { type_: PhotoType { data: mime(&path).into() }, binval: Binval { data: data.clone() } }),
                        payloads: vec![],
                    };
                    match net.iq(None, IqRequest::Set(vcard.into())).await {
                        Ok(_) => {
                            let _ = tokio::fs::write(store::avatar_path(&dir, &me), data).await;
                            net.emit(Ev::Avatar { jid: me });
                        }
                        Err(e) => net.emit(Ev::Error(format!("Не удалось сменить аватар: {e}"))),
                    }
                });
            }
            Cmd::Call(c) => self.calls.on_cmd(c),
            Cmd::E2eInfo { jid } => {
                let (Some(e2e), net) = (self.e2e.clone(), self.net.clone()) else { return };
                tokio::spawn(async move {
                    // Refresh from the server before showing the list.
                    let _ = e2e::has_devices(&e2e, &net, &jid).await;
                    let devices = e2e.lock().await.device_info(&jid);
                    net.emit(Ev::E2eDevices { jid, devices });
                });
            }
            Cmd::PgpGenerate | Cmd::PgpImportSecret { .. } | Cmd::PgpDeleteSecret => {
                let (Some(e2e), net, me) = (self.e2e.clone(), self.net.clone(), self.me.to_string()) else { return };
                let sig = self.pgp_presence.clone();
                let presence = self.presence();
                let cmd_kind = match cmd {
                    Cmd::PgpGenerate => Ok(None),
                    Cmd::PgpImportSecret { armored } => Ok(Some(armored)),
                    _ => Err(()),
                };
                tokio::spawn(async move {
                    let key = match cmd_kind {
                        Ok(None) => match pgp::generate(&format!("<xmpp:{me}>")) {
                            Ok(k) => Some(k),
                            Err(e) => return net.emit(Ev::Error(format!("Не удалось создать ключ: {e}"))),
                        },
                        Ok(Some(armored)) => match pgp::parse_secret(&armored) {
                            Ok(k) => Some(k),
                            Err(e) => return net.emit(Ev::Error(e)),
                        },
                        Err(()) => None,
                    };
                    *sig.lock().unwrap() = key.as_ref().and_then(|k| pgp::sign_status("", k).ok());
                    let own = key.as_ref().map(|k| (pgp::fingerprint(&pgp::public_of(k)), pgp::armor_public(&pgp::public_of(k))));
                    e2e.lock().await.pgp_set_secret(key);
                    // Re-announce (or stop announcing) our key in presence.
                    let mut p = presence;
                    p.payloads.retain(|e| !e.is("x", pgp::NS_SIGNED));
                    if let Some(s) = sig.lock().unwrap().clone() {
                        p.payloads.push(Element::builder("x", pgp::NS_SIGNED).append(s).build());
                    }
                    net.send(p);
                    net.emit(Ev::PgpOwn { fingerprint: own.as_ref().map(|o| o.0.clone()), public: own.map(|o| o.1) });
                });
            }
            Cmd::PgpImportContact { jid, armored } => {
                let (Some(e2e), net) = (self.e2e.clone(), self.net.clone()) else { return };
                tokio::spawn(async move {
                    match pgp::parse_public(&armored) {
                        Ok(k) => {
                            e2e.lock().await.pgp_set_contact(&jid, &k);
                            net.emit(Ev::PgpContact { jid, fingerprint: Some(pgp::fingerprint(&k)), hint: None });
                        }
                        Err(e) => net.emit(Ev::Error(e)),
                    }
                });
            }
            Cmd::PgpInfo { jid } => {
                let (Some(e2e), net) = (self.e2e.clone(), self.net.clone()) else { return };
                tokio::spawn(async move {
                    let e = e2e.lock().await;
                    let own = e.pgp_secret.as_ref().map(pgp::public_of);
                    net.emit(Ev::PgpOwn { fingerprint: own.as_ref().map(pgp::fingerprint), public: own.as_ref().map(pgp::armor_public) });
                    if let Some(jid) = jid {
                        let fingerprint = e.pgp_contact(&jid).map(|k| pgp::fingerprint(&k));
                        let hint = e.pgp_hints.get(&jid).cloned();
                        net.emit(Ev::PgpContact { jid, fingerprint, hint });
                    }
                });
            }
            Cmd::E2eBlindTrust(on) => {
                if let Some(e2e) = self.e2e.clone() {
                    tokio::spawn(async move { e2e.lock().await.blind_trust = on });
                }
            }
            Cmd::E2eTrust { jid, device, trusted } => {
                let (Some(e2e), net) = (self.e2e.clone(), self.net.clone()) else { return };
                tokio::spawn(async move {
                    let mut e = e2e.lock().await;
                    e.set_trusted(&jid, device, trusted);
                    let devices = e.device_info(&jid);
                    net.emit(Ev::E2eDevices { jid, devices });
                });
            }
        }
    }

    fn join(&mut self, room: String, nick: String, since: Option<i64>, create_name: Option<String>) {
        let Some(r) = jid(&room) else { return };
        let Ok(full) = r.with_resource_str(&nick) else { return };
        let history = match since.and_then(chrono::DateTime::from_timestamp_millis) {
            // +1s: `since` is inclusive and we already have that message.
            Some(t) => History::new().with_since(tokio_xmpp::parsers::date::DateTime((t + chrono::Duration::seconds(1)).fixed_offset())),
            None => History::new().with_maxstanzas(50),
        };
        self.rooms.insert(r, Room { nick, occupants: HashSet::new(), joined: false, create_name });
        self.net.send(self.presence().with_to(full).with_payload(Muc::new().with_history(history)));
    }

    /// Text message, encrypted per `enc`; `replace` makes it an XEP-0308 correction.
    fn send_text(&self, to: String, id: String, body: String, groupchat: bool, enc: String, replace: Option<String>) {
        let fix = move |m: Message| match &replace {
            Some(r) => m.with_payload(Replace { id: Id(r.clone()) }),
            None => m,
        };
        let Some(e2e) = self.e2e.clone().filter(|_| !groupchat && enc != "plain") else {
            return self.net.send(fix(chat_message(&to, &id, body, None, groupchat)));
        };
        let net = self.net.clone();
        tokio::spawn(async move {
            if enc == "pgp" {
                match pgp_encrypt(&e2e, &to, &body).await {
                    Ok(payload) => {
                        net.send(fix(pgp_message(&to, &id, payload)));
                        net.emit(Ev::SentEncrypted { peer: to, id, enc: 2 });
                    }
                    Err(error) => net.emit(Ev::SendFailed { peer: to, id, error }),
                }
                return;
            }
            match encrypt_for(&e2e, &net, &to, &enc, &body).await {
                Ok(Some(el)) => {
                    net.send(fix(encrypted_message(&to, &id, el)));
                    net.emit(Ev::SentEncrypted { peer: to, id, enc: 1 });
                }
                Ok(None) => net.send(fix(chat_message(&to, &id, body, None, false))),
                Err(error) => net.emit(Ev::SendFailed { peer: to, id, error }),
            }
        });
    }

    fn reply_iq(&self, iq: Iq) {
        let response = match iq {
            Iq::Get { from, id, payload, .. } => {
                if payload.is("query", ns::DISCO_INFO) {
                    let node = DiscoInfoQuery::try_from(payload).ok().and_then(|q| q.node);
                    Iq::Result { from: None, to: from, id, payload: Some(disco_info(node).into()) }
                } else if Ping::try_from(payload).is_ok() {
                    Iq::Result { from: None, to: from, id, payload: None }
                } else {
                    iq_error(from, id)
                }
            }
            Iq::Set { from, id, payload, .. } => {
                let from_server = from.as_ref().is_none_or(|f| f.to_bare() == self.me);
                if payload.is("query", ns::ROSTER) && from_server {
                    if let Ok(roster) = Roster::try_from(payload) {
                        for item in &roster.items {
                            self.net.emit(Ev::RosterItem(contact(item)));
                        }
                    }
                    Iq::Result { from: None, to: from, id, payload: None }
                } else if payload.is("jingle", ns::JINGLE) {
                    match self.calls.on_jingle(from.clone(), payload) {
                        Ok(()) => Iq::Result { from: None, to: from, id, payload: None },
                        Err(cond) => Iq::Error {
                            from: None,
                            to: from,
                            id,
                            error: StanzaError::new(ErrorType::Cancel, cond, "en", ""),
                            payload: None,
                        },
                    }
                } else {
                    iq_error(from, id)
                }
            }
            // Responses to our own IQs are routed by tokio-xmpp; late ones end up here.
            Iq::Result { .. } | Iq::Error { .. } => return,
        };
        self.net.send(response);
    }

    fn on_stanza(&mut self, stanza: Stanza) {
        match stanza {
            Stanza::Iq(iq) => self.reply_iq(iq),
            Stanza::Presence(p) => self.on_presence(p),
            Stanza::Message(m) => self.on_message(m, None, 0),
        }
    }

    fn on_presence(&mut self, p: Presence) {
        let Some(from) = p.from.clone() else { return };
        let bare = from.to_bare();
        if let Some(room) = self.rooms.get_mut(&bare) {
            let nick = from.resource().map(|r| r.to_string()).unwrap_or_default();
            if p.type_ == PresenceType::Error {
                let error = p
                    .payloads
                    .iter()
                    .find_map(|e| StanzaError::try_from(e.clone()).ok())
                    .map(|e| format!("{:?}", e.defined_condition))
                    .unwrap_or_else(|| "ошибка".into());
                self.rooms.remove(&bare);
                self.net.emit(Ev::RoomError { room: bare.to_string(), error });
                return;
            }
            let muc = p.payloads.iter().find_map(|e| MucUser::try_from(e.clone()).ok());
            let statuses = muc.map(|m| m.status).unwrap_or_default();
            if nick.is_empty() {
                // presence from the room itself, not an occupant
            } else if p.type_ == PresenceType::Unavailable {
                room.occupants.remove(&nick);
            } else {
                room.occupants.insert(nick);
            }
            if statuses.contains(&MucStatus::RoomHasBeenCreated) {
                // New room: unlock it as persistent so it survives everyone leaving.
                let name = room.create_name.clone().unwrap_or_default();
                let net = self.net.clone();
                let to = bare.clone();
                tokio::spawn(async move {
                    let form = room_config_form(&name);
                    if let Err(e) = net.iq(Some(to.into()), IqRequest::Set(form)).await {
                        net.emit(Ev::Error(format!("Не удалось настроить группу: {e}")));
                    }
                });
            }
            if statuses.contains(&MucStatus::SelfPresence) && !room.joined {
                room.joined = true;
                let (net, to) = (self.net.clone(), bare.clone());
                tokio::spawn(async move {
                    if let Ok(Some(el)) = net.iq(Some(to.clone().into()), IqRequest::Get(DiscoInfoQuery { node: None }.into())).await
                        && let Ok(info) = DiscoInfoResult::try_from(el)
                        && let Some(name) = info.identities.into_iter().find_map(|i| i.name).filter(|n| !n.is_empty())
                    {
                        net.emit(Ev::RoomName { room: to.to_string(), name });
                    }
                });
            }
            if room.joined {
                self.net.emit(Ev::RoomJoined { room: bare.to_string(), occupants: room.occupants.len() });
            }
            return;
        }
        match p.type_ {
            PresenceType::Subscribe => self.net.emit(Ev::SubscribeRequest { jid: bare.to_string() }),
            PresenceType::None if bare != self.me && p.payloads.iter().any(|e| e.is("x", pgp::NS_SIGNED)) && self.pgp_seen.insert(bare.to_string()) => {
                // XEP-0027: learn the contact's key id and try to fetch a key that verifies their signature.
                let sig = p.payloads.iter().find(|e| e.is("x", pgp::NS_SIGNED)).map(|e| e.text()).unwrap_or_default();
                let status = p.statuses.values().next().cloned().unwrap_or_default();
                if let (Some(e2e), Some(key_id)) = (self.e2e.clone(), pgp::signature_issuer(&sig)) {
                    let (net, http, jid) = (self.net.clone(), self.http.clone(), bare.to_string());
                    tokio::spawn(async move {
                        let known = {
                            let mut e = e2e.lock().await;
                            e.pgp_hints.insert(jid.clone(), key_id.clone());
                            e.pgp_contact(&jid).is_some()
                        };
                        if known {
                            return;
                        }
                        let url = format!("https://keys.openpgp.org/vks/v1/by-keyid/{key_id}");
                        let Ok(resp) = http.get(&url).send().await.and_then(|r| r.error_for_status()) else { return };
                        let Ok(armored) = resp.text().await else { return };
                        if let Ok(key) = pgp::parse_public(&armored)
                            && pgp::verify_status(&status, &sig, &key)
                        {
                            e2e.lock().await.pgp_set_contact(&jid, &key);
                            net.emit(Ev::PgpContact { jid, fingerprint: Some(pgp::fingerprint(&key)), hint: None });
                        }
                    });
                }
                self.on_presence(p);
            }
            PresenceType::None | PresenceType::Unavailable if bare != self.me => {
                let res = self.resources.entry(bare.clone()).or_default();
                let resource = from.resource().map(|r| r.to_string()).unwrap_or_default();
                if p.type_ == PresenceType::None {
                    res.insert(resource);
                } else {
                    res.remove(&resource);
                }
                let online = !res.is_empty();
                self.net.emit(Ev::Presence { jid: bare.to_string(), online });
                if p.type_ == PresenceType::None && p.payloads.iter().any(|e| e.is("x", ns::VCARD_UPDATE)) {
                    let (net, dir, j) = (self.net.clone(), self.dir.clone(), bare.to_string());
                    tokio::spawn(async move { fetch_avatar(&net, &dir, &j).await });
                }
            }
            _ => {}
        }
    }

    /// `archived` = (archive id, timestamp) when the message came out of MAM.
    /// `enc` > 0: second pass of a message we just decrypted (1 = OMEMO).
    fn on_message(&mut self, m: Message, archived: Option<(String, Option<i64>)>, enc: u8) {
        let from = m.from.clone();
        // Unwrap MAM results and carbons; they must come from our own account.
        let from_self = from.as_ref().is_none_or(|f| f.to_bare() == self.me);
        for p in &m.payloads {
            if let Ok(r) = mam::Result_::try_from(p.clone()) {
                let ts = r.forwarded.delay.as_ref().map(|d| d.stamp.0.timestamp_millis());
                let from_room = from.as_ref().is_some_and(|f| self.rooms.contains_key(&f.to_bare()));
                if from_self || from_room {
                    self.on_message(r.forwarded.message, Some((r.id, ts)), enc);
                }
                return;
            }
            if from_self {
                if let Ok(c) = carbons::Received::try_from(p.clone()) {
                    return self.on_message(c.forwarded.message, None, enc);
                }
                if let Ok(c) = carbons::Sent::try_from(p.clone()) {
                    return self.on_message(c.forwarded.message, None, enc);
                }
            }
        }

        if m.type_ == MessageType::Error {
            if let (Some(id), Some(f)) = (&m.id, &from) {
                let reason = m
                    .payloads
                    .iter()
                    .find_map(|p| StanzaError::try_from(p.clone()).ok())
                    .map(|e| format!("{:?}{}", e.defined_condition, e.texts.values().next().map(|t| format!(": {t}")).unwrap_or_default()))
                    .unwrap_or_default();
                self.net.emit(Ev::SendFailed { peer: f.to_bare().to_string(), id: id.0.clone(), error: format!("сервер отклонил сообщение {reason}") });
            }
            return;
        }
        let Some(from) = from else { return };
        let outgoing_carbon = from.to_bare() == self.me;

        // OMEMO device list updates (PEP notifications).
        if let Some(ids) = e2e::devicelist_event(&m.payloads) {
            if let Some(e2e) = self.e2e.clone() {
                tokio::spawn(e2e::on_devicelist(e2e, self.net.clone(), from.to_bare().to_string(), ids));
            }
            return;
        }
        // XEP-0402 bookmark changes from our other devices; only our own account may send them.
        if let Some(items) = m.payloads.iter().find(|p| p.is("event", e2e::NS_PUBSUB_EVENT)).and_then(|e| e.get_child("items", e2e::NS_PUBSUB_EVENT))
            && items.attr("node") == Some(ns::BOOKMARKS2)
        {
            if outgoing_carbon {
                for c in items.children() {
                    match (c.name(), bookmark(c)) {
                        ("item", Some(b)) => self.net.emit(Ev::BookmarkAdded(b)),
                        // autojoin=false means "left" for other clients.
                        ("item" | "retract", None) => {
                            if let Some(room) = c.attr("id") {
                                self.net.emit(Ev::BookmarkRemoved { room: room.to_string() });
                            }
                        }
                        _ => {}
                    }
                }
            }
            return;
        }
        let groupchat = m.type_ == MessageType::Groupchat;

        // Calls: XEP-0353 propose/proceed/accept/reject/retract.
        if let Some(el) = m.payloads.iter().find(|e| e.ns() == ns::JINGLE_MESSAGE) {
            self.calls.on_jmi(&from, m.to.as_ref(), el.clone(), archived.is_some());
            return;
        }

        // Invites (mediated XEP-0045 and direct XEP-0249).
        for p in &m.payloads {
            if let Ok(u) = MucUser::try_from(p.clone())
                && let Some(inv) = u.invite
            {
                let inviter = inv.from.map(|j| j.to_bare().to_string()).unwrap_or_default();
                self.net.emit(Ev::Invite { room: from.to_bare().to_string(), from: inviter });
                return;
            }
            if p.is("x", "jabber:x:conference")
                && let Some(room) = p.attr("jid")
            {
                self.net.emit(Ev::Invite { room: room.to_string(), from: from.to_bare().to_string() });
                return;
            }
        }

        let (peer, sender, outgoing) = if groupchat {
            let room = from.to_bare();
            let nick = from.resource().map(|r| r.to_string()).unwrap_or_default();
            let mine = self.rooms.get(&room).is_some_and(|r| r.nick == nick);
            (room.to_string(), nick, mine)
        } else if outgoing_carbon {
            let Some(to) = m.to.as_ref() else { return };
            (to.to_bare().to_string(), self.me.to_string(), true)
        } else {
            (from.to_bare().to_string(), from.to_bare().to_string(), false)
        };
        let live = archived.is_none();

        // Already handled on the first (encrypted) pass.
        for p in m.payloads.iter().filter(|_| enc == 0) {
            if let Ok(r) = Received::try_from(p.clone()) {
                self.net.emit(Ev::Receipt { peer: peer.clone(), id: r.id });
            } else if let Ok(d) = Displayed::try_from(p.clone()) {
                self.net.emit(Ev::Displayed { peer: peer.clone(), id: d.id.0, by_me: outgoing });
            } else if live
                && !outgoing
                && let Ok(state) = ChatState::try_from(p.clone())
            {
                let on = state == ChatState::Composing;
                self.net.emit(Ev::Typing { peer: peer.clone(), who: sender.clone(), on });
            }
        }
        if let Some(r) = m.payloads.iter().find_map(|p| Reactions::try_from(p.clone()).ok()) {
            let emojis = r.reactions.into_iter().map(|x| x.emoji).collect();
            self.net.emit(Ev::Reactions { peer, id: r.id, sender, emojis });
            return;
        }

        // OMEMO: decrypt off the loop, then process the plaintext as a normal message.
        if enc == 0
            && let Some(el) = m.payloads.iter().find(|p| p.is("encrypted", omemo::NS)).cloned()
        {
            let Some(e2e) = self.e2e.clone() else { return };
            let net = self.net.clone();
            let sender = if outgoing_carbon { self.me.to_string() } else { from.to_bare().to_string() };
            tokio::spawn(async move {
                let (text, enc) = match e2e::decrypt(&e2e, &net, &sender, &el).await {
                    Ok(Some(text)) => (text, 1),
                    Ok(None) => return, // key transport / session completion
                    Err(e) if e == e2e::NOT_FOR_THIS_DEVICE => ("🔒 Сообщение зашифровано для другого устройства — здесь его не прочитать".to_string(), 3),
                    Err(e) => (format!("🔒 Не удалось расшифровать сообщение OMEMO: {e}"), 3),
                };
                let mut plain = m;
                plain.bodies.clear();
                plain.bodies.insert(Lang::default(), text);
                plain.payloads.retain(|p| !p.is("encrypted", omemo::NS) && !p.is("encryption", e2e::NS_EME));
                net.inject(plain, archived, enc);
            });
            return;
        }
        // XEP-0027: decrypt with our OpenPGP key.
        if enc == 0
            && let Some(payload) = m.payloads.iter().find(|p| p.is("x", pgp::NS_ENCRYPTED)).map(|p| p.text())
        {
            let Some(e2e) = self.e2e.clone() else { return };
            let net = self.net.clone();
            tokio::spawn(async move {
                let res = match e2e.lock().await.pgp_secret.clone() {
                    Some(key) => tokio::task::spawn_blocking(move || pgp::decrypt(&payload, &key)).await.unwrap_or_else(|e| Err(e.to_string())),
                    None => Err("нет вашего ключа OpenPGP — добавьте его в настройках".into()),
                };
                let (text, enc) = match res {
                    Ok(t) => (t, 2),
                    Err(e) => (format!("🔒 Не удалось расшифровать сообщение OpenPGP: {e}"), 3),
                };
                let mut plain = m;
                plain.bodies.clear();
                plain.bodies.insert(Lang::default(), text);
                plain.payloads.retain(|p| !p.is("x", pgp::NS_ENCRYPTED) && !p.is("encryption", e2e::NS_EME));
                net.inject(plain, archived, enc);
            });
            return;
        }
        let Some(mut body) = msg_body(&m) else { return };
        // Encrypted with something we cannot read: show that instead of the fallback text.
        if enc == 0
            && let Some(ns) = m.payloads.iter().find(|p| p.is("encryption", e2e::NS_EME)).and_then(|p| p.attr("namespace"))
        {
            body = format!("🔒 Сообщение зашифровано ({}) — этот протокол не поддерживается", e2e::eme_name(ns));
        }
        let mut url = m.payloads.iter().find_map(|p| Oob::try_from(p.clone()).ok()).map(|o| o.url);
        if url.is_none() && body.starts_with("aesgcm://") && !body.contains(char::is_whitespace) {
            url = Some(body.clone());
        }
        if let Some(u) = &url {
            body = u.clone();
        }
        // 3 = decryption failed: stored as a plain notice.
        let enc = if enc == 3 { 0 } else { enc };

        // Ack delivery for live 1:1 messages that ask for it.
        if live && !groupchat && !outgoing && m.payloads.iter().any(|p| p.is("request", ns::RECEIPTS))
            && let Some(id) = &m.id
        {
            self.net.send(Message::chat(Some(from.clone())).with_payload(Received { id: id.0.clone() }));
        }
        if let Some(r) = m.payloads.iter().find_map(|p| Replace::try_from(p.clone()).ok()) {
            if url.is_none() {
                self.net.emit(Ev::Correct { peer, id: r.id.0, sender, body });
            }
            return;
        }

        let delay_ts = m.payloads.iter().find_map(|p| Delay::try_from(p.clone()).ok()).map(|d| d.stamp.0.timestamp_millis());
        let ts = archived.as_ref().and_then(|a| a.1).or(delay_ts).unwrap_or_else(now_ms);
        let stanza_ids: Vec<StanzaId> = m.payloads.iter().filter_map(|p| StanzaId::try_from(p.clone()).ok()).collect();
        let stanza_id = stanza_ids.iter().find(|s| s.by.to_bare() == self.me).map(|s| s.id.clone());
        let sid = stanza_ids.iter().find(|s| groupchat && s.by.to_bare() == from.to_bare()).map(|s| s.id.clone());
        let archive_id = if groupchat { None } else { archived.as_ref().map(|a| a.0.clone()).or(stanza_id) };
        let origin = m.payloads.iter().find_map(|p| OriginId::try_from(p.clone()).ok()).map(|o| o.id);
        let msg_id = origin
            .or(m.id.as_ref().map(|i| i.0.clone()))
            .or(archived.as_ref().map(|a| a.0.clone()))
            .unwrap_or_else(new_id);

        self.net.emit(Ev::Message {
            peer,
            groupchat,
            msg: Msg { msg_id, sender, outgoing, body, url, ts, status: if outgoing { 1 } else { 0 }, enc, edited: false },
            archive_id,
            live: live && delay_ts.is_none(),
            sid,
        });
    }
}

/// Standard outgoing chat/groupchat message with receipts, markers and origin-id.
fn chat_message(to: &str, id: &str, body: String, url: Option<String>, groupchat: bool) -> Message {
    let mut m = Message::new_with_type(if groupchat { MessageType::Groupchat } else { MessageType::Chat }, jid(to).map(Jid::from))
        .with_body(Lang::default(), body)
        .with_payload(ChatState::Active)
        .with_payload(OriginId { id: id.to_string() });
    m.id = Some(Id(id.to_string()));
    if !groupchat {
        m = m.with_payload(ReceiptRequest).with_payload(Markable);
    }
    if let Some(url) = url {
        m = m.with_payload(Oob { url, desc: None });
    }
    m
}

/// OMEMO message: encrypted element + EME + store hint, with the standard fallback body.
fn encrypted_message(to: &str, id: &str, encrypted: Element) -> Message {
    let eme = Element::builder("encryption", e2e::NS_EME).attr(nc("namespace"), omemo::NS).attr(nc("name"), "OMEMO").build();
    let hint = Element::builder("store", "urn:xmpp:hints").build();
    chat_message(to, id, omemo::FALLBACK_BODY.to_string(), None, false).with_payloads(vec![encrypted, eme, hint])
}

/// XEP-0027 payload for `to` (and our own key, so other devices / the archive stay readable).
async fn pgp_encrypt(e2e: &e2e::Shared, to: &str, body: &str) -> Result<String, String> {
    let (theirs, ours) = {
        let e = e2e.lock().await;
        let ours = e.pgp_secret.as_ref().map(pgp::public_of).ok_or("сначала создайте или импортируйте свой ключ OpenPGP в настройках")?;
        (e.pgp_contact(to).ok_or("нет открытого ключа собеседника — импортируйте его в настройках шифрования чата")?, ours)
    };
    let body = body.to_string();
    tokio::task::spawn_blocking(move || pgp::encrypt(&body, &[&theirs, &ours])).await.map_err(|e| e.to_string())?
}

fn pgp_message(to: &str, id: &str, payload: String) -> Message {
    let x = Element::builder("x", pgp::NS_ENCRYPTED).append(payload).build();
    let eme = Element::builder("encryption", e2e::NS_EME).attr(nc("namespace"), pgp::NS_ENCRYPTED).attr(nc("name"), "OpenPGP").build();
    let hint = Element::builder("store", "urn:xmpp:hints").build();
    chat_message(to, id, pgp::FALLBACK_BODY.to_string(), None, false).with_payloads(vec![x, eme, hint])
}

/// Decides OMEMO for a send. `Ok(None)`: send in plain text (auto mode and the contact has no OMEMO).
async fn encrypt_for(e2e: &e2e::Shared, net: &Net, to: &str, enc: &str, body: &str) -> Result<Option<Element>, String> {
    let want = match enc {
        "omemo" => true,
        "pgp" => return Err("XEP-0027 не поддерживает файлы — переключите чат на OMEMO, чтобы отправлять их зашифрованными".into()),
        _ => e2e::has_devices(e2e, net, to).await,
    };
    if !want {
        return Ok(None);
    }
    e2e::encrypt(e2e, net, to, body).await.map(Some)
}

pub fn nc(s: &str) -> tokio_xmpp::minidom::rxml::NcName {
    s.try_into().unwrap()
}

fn iq_error(to: Option<Jid>, id: String) -> Iq {
    Iq::Error {
        from: None,
        to,
        id,
        error: StanzaError::new(ErrorType::Cancel, DefinedCondition::ServiceUnavailable, "en", ""),
        payload: None,
    }
}

fn contact(i: &RosterItem) -> Contact {
    let subscription = match i.subscription {
        Subscription::None => "none",
        Subscription::From => "from",
        Subscription::To => "to",
        Subscription::Both => "both",
        Subscription::Remove => "remove",
    };
    Contact { jid: i.jid.to_string(), name: i.name.clone().unwrap_or_default(), subscription: subscription.into() }
}

/// Pubsub item of the XEP-0402 node (`<item id=room><conference/></item>`); `None` unless autojoin.
fn bookmark(item: &Element) -> Option<Bookmark> {
    let room = item.attr("id")?;
    let conf = Conference::try_from(item.get_child("conference", ns::BOOKMARKS2)?.clone()).ok()?;
    conf.autojoin.then(|| Bookmark { room: room.to_string(), name: conf.name.unwrap_or_default(), nick: conf.nick.map(|n| n.to_string()).unwrap_or_default() })
}

fn room_config_form(name: &str) -> Element {
    let field = |var: &str, value: &str| {
        Element::builder("field", ns::DATA_FORMS)
            .attr(nc("var"), var)
            .append(Element::builder("value", ns::DATA_FORMS).append(value))
    };
    let mut x = Element::builder("x", ns::DATA_FORMS)
        .attr(nc("type"), "submit")
        .append(field("FORM_TYPE", "http://jabber.org/protocol/muc#roomconfig"))
        .append(field("muc#roomconfig_persistentroom", "1"))
        .append(field("muc#roomconfig_publicroom", "0"))
        .append(field("muc#roomconfig_whois", "anyone"))
        .append(field("mam", "1"));
    if !name.is_empty() {
        x = x.append(field("muc#roomconfig_roomname", name));
    }
    Element::builder("query", "http://jabber.org/protocol/muc#owner").append(x).build()
}

async fn mam_sync(net: Net, mut after: Option<String>) {
    // First login: only the newest page. Later: everything after the last seen archive id.
    let first = after.is_none();
    for _ in 0..50 {
        let set = if first {
            SetQuery { max: Some(100), after: None, before: Some(String::new()), index: None }
        } else {
            SetQuery { max: Some(100), after: after.clone(), before: None, index: None }
        };
        let query = mam::Query { queryid: Some(QueryId(new_id())), node: None, form: None, set: Some(set), flip_page: false };
        let Ok(Some(el)) = net.iq(None, IqRequest::Set(query.into())).await else { return };
        let Ok(fin) = mam::Fin::try_from(el) else { return };
        if first || fin.complete || fin.set.last.is_none() {
            return;
        }
        after = fin.set.last;
    }
}

async fn fetch_avatar(net: &Net, dir: &std::path::Path, who: &str) {
    let Some(to) = jid(who) else { return };
    if let Ok(Some(el)) = net.iq(Some(to.into()), IqRequest::Get(VCardQuery.into())).await
        && let Ok(vcard) = VCard::try_from(el)
        && let Some(photo) = vcard.photo
        && tokio::fs::write(store::avatar_path(dir, who), photo.binval.data).await.is_ok()
    {
        net.emit(Ev::Avatar { jid: who.to_string() });
    }
}

async fn discover_upload(net: &Net, domain: &str) -> Option<Jid> {
    let server = Jid::from_str(domain).ok()?;
    let items = net.iq(Some(server.clone()), IqRequest::Get(DiscoItemsQuery { node: None, rsm: None }.into())).await.ok()??;
    let items = DiscoItemsResult::try_from(items).ok()?;
    for candidate in std::iter::once(server).chain(items.items.into_iter().map(|i| i.jid)) {
        if let Ok(Some(el)) = net.iq(Some(candidate.clone()), IqRequest::Get(DiscoInfoQuery { node: None }.into())).await
            && let Ok(info) = DiscoInfoResult::try_from(el)
            && info.features.contains(ns::HTTP_UPLOAD)
        {
            return Some(candidate);
        }
    }
    None
}

fn mime(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("txt") => "text/plain",
        Some("pdf") => "application/pdf",
        Some("mp3") => "audio/mpeg",
        Some("mp4") => "video/mp4",
        _ => "application/octet-stream",
    }
}

async fn upload_file(net: &Net, http: &reqwest::Client, upload_service: Arc<Mutex<Option<Jid>>>, path: &std::path::Path) -> Result<String, String> {
    let service = upload_service.lock().unwrap().clone().ok_or("сервер не поддерживает загрузку файлов")?;
    drop(service);
    // shortcut: the whole file is read into memory; switch to a streamed body for multi-GB uploads.
    let data = tokio::fs::read(path).await.map_err(|e| e.to_string())?;
    let filename = path.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
    upload_bytes(net, http, upload_service, data, filename, mime(path)).await
}

async fn upload_bytes(net: &Net, http: &reqwest::Client, service: Arc<Mutex<Option<Jid>>>, data: Vec<u8>, filename: String, content_type: &str) -> Result<String, String> {
    let service = service.lock().unwrap().clone().ok_or("сервер не поддерживает загрузку файлов")?;
    let req = SlotRequest { filename, size: data.len() as u64, content_type: Some(content_type.into()) };
    let slot = net.iq(Some(service), IqRequest::Get(req.into())).await?.ok_or("пустой ответ")?;
    let slot = SlotResult::try_from(slot).map_err(|e| e.to_string())?;
    let mut put = http.put(&slot.put.url).header("Content-Type", content_type).body(data);
    for Header { name, value } in slot.put.headers {
        put = put.header(name.as_str(), value);
    }
    put.send().await.and_then(|r| r.error_for_status()).map_err(|e| e.to_string())?;
    Ok(slot.get.url)
}

#[cfg(test)]
mod caps_tests {
    #[test]
    fn caps_string_uses_octet_order() {
        let s = super::caps_string(&super::disco_info(None));
        assert!(s.starts_with("client/pc/en/Crabber<"));
        assert!(s.contains("<eu.siacs.conversations.axolotl<eu.siacs.conversations.axolotl.devicelist+notify<"), "{s}");
    }

    #[test]
    fn bookmark_items() {
        let item = |autojoin: &str| -> super::Element {
            format!("<item xmlns='http://jabber.org/protocol/pubsub#event' id='r@muc.x'><conference xmlns='urn:xmpp:bookmarks:1' autojoin='{autojoin}' name='Room'><nick>me</nick></conference></item>").parse().unwrap()
        };
        let b = super::bookmark(&item("true")).unwrap();
        assert_eq!((b.room.as_str(), b.name.as_str(), b.nick.as_str()), ("r@muc.x", "Room", "me"));
        assert!(super::bookmark(&item("false")).is_none(), "not autojoin = left");
    }
}
