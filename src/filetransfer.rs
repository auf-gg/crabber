//! XEP-0234 Jingle File Transfer over XEP-0261 In-Band Bytestreams (XEP-0047): one file per
//! session, 1:1 and unencrypted. Sending falls back to it when the server has no HTTP Upload.
//! An offer from someone else becomes an attachment with a `jingle://` URL; fetching it like any
//! other download accepts the session.
//!
//! Interop notes: Conversations and Dino speak file-transfer:5 with ibb:1 only through a direct
//! session-initiate to a full JID (no XEP-0353). Conversations names the type `<mediaType>` and
//! does not wrap the IBB sequence after 65535; both are accepted here.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use futures::channel::oneshot;
use futures::{StreamExt, TryStreamExt};
use sha2::{Digest, Sha256};
use tokio_xmpp::IqRequest;
use tokio_xmpp::minidom::Element;
use tokio_xmpp::parsers::{
    disco::{DiscoInfoQuery, DiscoInfoResult},
    jid::{BareJid, Jid},
    ns,
    stanza_error::DefinedCondition,
};

use crate::call::JingleError;
use crate::store::Msg;
use crate::t;
use crate::xmpp::{Ev, Net, nc, new_id};

pub const NS_FT: &str = "urn:xmpp:jingle:apps:file-transfer:5";
pub const NS_JINGLE_IBB: &str = "urn:xmpp:jingle:transports:ibb:1";
pub const NS_IBB: &str = "http://jabber.org/protocol/ibb";
const NS_HASHES: &str = "urn:xmpp:hashes:2";
/// Id prefix of the chat line an offer becomes: local only, never marked read or counted unread.
pub const MSG_PREFIX: &str = "jingle-";
const URL_SCHEME: &str = "jingle://";
/// What we offer: Conversations takes up to 8192, Dino up to 65535.
const BLOCK: usize = 8192;
const MAX_BLOCK: usize = 65535;
/// Same cap as HTTP downloads: the file is held in memory until complete.
pub const MAX_SIZE: u64 = 100 * 1024 * 1024;
/// Blocks in flight at once: one IQ per round trip would crawl.
const WINDOW: usize = 8;
/// The other side has to click to accept.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The attachment URL of an offer: `jingle://<sid>/<name>`.
pub fn url(sid: &str, name: &str) -> String {
    // The name is the last path segment: nothing in it may end it early.
    let name: String = name.chars().map(|c| if matches!(c, '/' | '\\' | '?' | '#') { '_' } else { c }).collect();
    format!("{URL_SCHEME}{sid}/{name}")
}

/// The session id of a `jingle://` URL.
pub fn sid_of(url: &str) -> Option<&str> {
    url.strip_prefix(URL_SCHEME)?.split('/').next()
}

/// What we were offered and, once accepted, what has arrived.
struct Incoming {
    from: Jid,
    url: String,
    /// The offered `<content>`, echoed in session-accept.
    content: Element,
    size: u64,
    /// SHA-256 from the offer or a later checksum (XEP-0234 §8).
    sha256: Option<Vec<u8>>,
    ibb_sid: String,
    block: usize,
    accepted: Option<Received>,
}

struct Received {
    path: PathBuf,
    data: Vec<u8>,
    /// Blocks so far; the next `seq` is this mod 65536 (or unwrapped, as Conversations sends).
    blocks: u32,
}

/// Our offer, waiting for the other side.
struct Outgoing {
    accept: Option<oneshot::Sender<Result<usize, String>>>,
    done: Option<oneshot::Sender<bool>>,
}

#[derive(Default)]
struct Sessions {
    incoming: HashMap<String, Incoming>,
    outgoing: HashMap<String, Outgoing>,
}

#[derive(Clone)]
pub struct FileTransfers {
    net: Net,
    bound: Arc<Mutex<Option<Jid>>>,
    sessions: Arc<Mutex<Sessions>>,
}

impl FileTransfers {
    pub fn new(net: Net) -> Self {
        Self { net, bound: Arc::default(), sessions: Arc::default() }
    }

    pub fn set_bound(&self, jid: Jid) {
        *self.bound.lock().unwrap() = Some(jid);
    }

    /// A file offer, or a request in one of our file sessions.
    pub fn owns(&self, jingle: &Element) -> bool {
        let sid = jingle.attr("sid").unwrap_or_default();
        let s = self.sessions.lock().unwrap();
        s.incoming.contains_key(sid)
            || s.outgoing.contains_key(sid)
            || (jingle.attr("action") == Some("session-initiate") && jingle.children().any(|c| c.get_child("description", NS_FT).is_some()))
    }

    /// Answered synchronously, like every Jingle request (see `call::check`).
    pub fn on_jingle(&self, from: Option<Jid>, el: Element) -> Result<(), JingleError> {
        let from = from.ok_or(JingleError::new(DefinedCondition::BadRequest, None))?;
        let sid = el.attr("sid").unwrap_or_default().to_string();
        let unknown = || JingleError::new(DefinedCondition::ItemNotFound, Some("unknown-session"));
        match el.attr("action").unwrap_or_default() {
            "session-initiate" => self.on_offer(from, sid, &el),
            "session-accept" => {
                let mut s = self.sessions.lock().unwrap();
                let o = s.outgoing.get_mut(&sid).ok_or_else(unknown)?;
                let block = el.children().find_map(|c| c.get_child("transport", NS_JINGLE_IBB)).and_then(|t| t.attr("block-size")).and_then(|b| b.parse().ok());
                if let Some(tx) = o.accept.take() {
                    let _ = tx.send(Ok(block.unwrap_or(BLOCK).clamp(1, BLOCK)));
                }
                Ok(())
            }
            "session-terminate" => {
                let reason = el.get_child("reason", ns::JINGLE).and_then(|r| r.children().next()).map(|c| c.name().to_string()).unwrap_or_default();
                let mut s = self.sessions.lock().unwrap();
                if let Some(mut o) = s.outgoing.remove(&sid) {
                    if let Some(tx) = o.accept.take() {
                        let _ = tx.send(Err(if reason == "decline" { t!("ft.declined").to_string() } else { t!("ft.failed", reason = reason) }));
                    }
                    if let Some(tx) = o.done.take() {
                        let _ = tx.send(reason == "success");
                    }
                } else if let Some(i) = s.incoming.remove(&sid) {
                    // Some senders end the session themselves once the data is out.
                    match i.accepted {
                        Some(r) if reason == "success" && r.data.len() as u64 == i.size => self.finish(&sid, i.url, i.sha256, r, None),
                        Some(_) => self.net.emit(Ev::DownloadFailed { url: i.url, error: t!("ft.failed", reason = reason) }),
                        None => {}
                    }
                } else {
                    return Err(unknown());
                }
                Ok(())
            }
            // A checksum (XEP-0234 §8.2) or a ping.
            "session-info" => {
                let mut s = self.sessions.lock().unwrap();
                if let Some(i) = s.incoming.get_mut(&sid) {
                    if let Some(h) = el.get_child("checksum", NS_FT).and_then(|c| c.get_child("file", NS_FT)).and_then(sha256_of) {
                        i.sha256 = Some(h);
                    }
                    Ok(())
                } else if s.outgoing.contains_key(&sid) {
                    Ok(())
                } else {
                    Err(unknown())
                }
            }
            _ => Err(JingleError::new(DefinedCondition::FeatureNotImplemented, None)),
        }
    }

    fn on_offer(&self, from: Jid, sid: String, el: &Element) -> Result<(), JingleError> {
        let bad = || JingleError::new(DefinedCondition::BadRequest, None);
        let content = el.children().find(|c| c.is("content", ns::JINGLE)).ok_or_else(bad)?;
        let file = content.get_child("description", NS_FT).and_then(|d| d.get_child("file", NS_FT)).ok_or_else(bad)?;
        let text = |name: &str| file.get_child(name, NS_FT).map(|e| e.text().trim().to_string()).filter(|s| !s.is_empty());
        let name = text("name").unwrap_or_else(|| t!("media.file_fallback").to_string());
        let size = text("size").and_then(|s| s.parse().ok()).ok_or_else(bad)?;
        let Some(transport) = content.get_child("transport", NS_JINGLE_IBB) else {
            // Only IBB here; Dino and Conversations end such sessions the same way.
            let terminate = jingle("session-terminate", &sid).append(Element::builder("reason", ns::JINGLE).append(Element::builder("unsupported-transports", ns::JINGLE))).build();
            let net = self.net.clone();
            tokio::spawn(async move {
                let _ = net.iq(Some(from), IqRequest::Set(terminate)).await;
            });
            return Ok(());
        };
        let incoming = Incoming {
            url: url(&sid, &name),
            content: content.clone(),
            size,
            sha256: sha256_of(file),
            ibb_sid: transport.attr("sid").unwrap_or_default().to_string(),
            block: transport.attr("block-size").and_then(|b| b.parse().ok()).unwrap_or(BLOCK).clamp(1, MAX_BLOCK),
            accepted: None,
            from: from.clone(),
        };
        let msg_url = incoming.url.clone();
        if self.sessions.lock().unwrap().incoming.insert(sid.clone(), incoming).is_some() {
            return Ok(()); // the same offer again
        }
        let peer = from.to_bare().to_string();
        let msg = Msg {
            msg_id: format!("{MSG_PREFIX}{sid}"),
            sender: peer.clone(),
            outgoing: false,
            body: msg_url.clone(),
            url: Some(msg_url),
            ts: chrono::Utc::now().timestamp_millis(),
            status: 0,
            enc: 0,
            edited: false,
            reply_to: None,
            retracted: false,
        };
        self.net.emit(Ev::Message { peer, groupchat: false, msg, archive_id: None, live: true, sid: None, occupant: None });
        Ok(())
    }

    /// Takes the offer behind `url`; the file lands at `path`.
    pub fn accept(&self, url: &str, path: PathBuf) {
        let fail = |error: String| self.net.emit(Ev::DownloadFailed { url: url.to_string(), error });
        let Some(sid) = sid_of(url) else { return fail(t!("file.unavailable").into()) };
        let mut s = self.sessions.lock().unwrap();
        let Some(i) = s.incoming.get_mut(sid).filter(|i| i.accepted.is_none()) else { return fail(t!("file.unavailable").into()) };
        if i.size > MAX_SIZE {
            drop(s);
            self.terminate(sid, "decline");
            self.sessions.lock().unwrap().incoming.remove(sid);
            return fail(t!("err.file_too_big").into());
        }
        let mut content = i.content.clone();
        content.remove_child("transport", NS_JINGLE_IBB);
        content.append_child(Element::builder("transport", NS_JINGLE_IBB).attr(nc("sid"), i.ibb_sid.as_str()).attr(nc("block-size"), i.block.to_string()).build());
        i.accepted = Some(Received { path, data: Vec::with_capacity(i.size as usize), blocks: 0 });
        let mut accept = jingle("session-accept", sid).append(content);
        if let Some(me) = self.bound.lock().unwrap().clone() {
            accept = accept.attr(nc("responder"), me.to_string());
        }
        let (net, to, url) = (self.net.clone(), i.from.clone(), url.to_string());
        tokio::spawn(async move {
            if let Err(e) = net.iq(Some(to), IqRequest::Set(accept.build())).await {
                net.emit(Ev::DownloadFailed { url, error: e });
            }
        });
    }

    /// XEP-0047 `<open>`, `<data>` or `<close>` from the sender of a file we accepted.
    pub fn on_ibb(&self, from: Option<Jid>, el: &Element) -> Result<(), DefinedCondition> {
        let ibb_sid = el.attr("sid").unwrap_or_default();
        let mut s = self.sessions.lock().unwrap();
        let Some((sid, i)) = s.incoming.iter_mut().find(|(_, i)| i.ibb_sid == ibb_sid && i.accepted.is_some() && Some(&i.from) == from.as_ref()) else {
            // A close after we already finished on the last block is fine.
            return if el.name() == "close" { Ok(()) } else { Err(DefinedCondition::ItemNotFound) };
        };
        let sid = sid.clone();
        let r = i.accepted.as_mut().unwrap();
        match el.name() {
            // Data over IQs only, as Conversations and Dino send it.
            "open" if el.attr("stanza") == Some("message") => Err(DefinedCondition::NotAcceptable),
            "open" => Ok(()),
            "data" => {
                let seq: u32 = el.attr("seq").and_then(|s| s.parse().ok()).ok_or(DefinedCondition::BadRequest)?;
                let chunk = base64::engine::general_purpose::STANDARD.decode(el.text().trim()).map_err(|_| DefinedCondition::BadRequest)?;
                let in_order = seq == r.blocks % 65536 || seq == r.blocks;
                if !in_order || chunk.len() > MAX_BLOCK || (r.data.len() + chunk.len()) as u64 > i.size {
                    let (url, from) = (i.url.clone(), i.from.clone());
                    s.incoming.remove(&sid);
                    drop(s);
                    self.net.emit(Ev::DownloadFailed { url, error: t!("ft.failed", reason = "data") });
                    self.terminate_to(&sid, from, "failed-transport");
                    return Err(DefinedCondition::UnexpectedRequest);
                }
                r.blocks += 1;
                r.data.extend_from_slice(&chunk);
                Ok(())
            }
            "close" => {
                let i = s.incoming.remove(&sid).unwrap();
                let r = i.accepted.unwrap();
                drop(s);
                if r.data.len() as u64 == i.size {
                    self.finish(&sid, i.url, i.sha256, r, Some(i.from));
                } else {
                    self.net.emit(Ev::DownloadFailed { url: i.url, error: t!("ft.failed", reason = "incomplete") });
                    self.terminate_to(&sid, i.from, "failed-transport");
                }
                Ok(())
            }
            _ => Err(DefinedCondition::BadRequest),
        }
    }

    /// All bytes are in: check them, write the file, and end the session with the sender
    /// (`from`; `None` when the sender ended it already).
    fn finish(&self, sid: &str, url: String, sha256: Option<Vec<u8>>, r: Received, from: Option<Jid>) {
        let ok = sha256.is_none_or(|h| Sha256::digest(&r.data).as_slice() == h);
        if let Some(from) = from {
            self.terminate_to(sid, from, if ok { "success" } else { "media-error" });
        }
        if !ok {
            return self.net.emit(Ev::DownloadFailed { url, error: t!("ft.corrupt").into() });
        }
        let net = self.net.clone();
        tokio::spawn(async move {
            match tokio::fs::write(&r.path, &r.data).await {
                Ok(()) => net.emit(Ev::Downloaded { url }),
                Err(e) => net.emit(Ev::DownloadFailed { url, error: e.to_string() }),
            }
        });
    }

    fn terminate(&self, sid: &str, reason: &str) {
        let to = self.sessions.lock().unwrap().incoming.get(sid).map(|i| i.from.clone());
        if let Some(to) = to {
            self.terminate_to(sid, to, reason);
        }
    }

    fn terminate_to(&self, sid: &str, to: Jid, reason: &str) {
        let el = jingle("session-terminate", sid).append(Element::builder("reason", ns::JINGLE).append(Element::builder(reason, ns::JINGLE))).build();
        let net = self.net.clone();
        tokio::spawn(async move {
            let _ = net.iq(Some(to), IqRequest::Set(el)).await;
        });
    }

    /// Sends `path` to the first of `resources` (online resources of `to`) that takes Jingle files.
    /// Reports like an upload: `Ev::Uploaded` with the `jingle://` URL, or `Ev::SendFailed`.
    pub async fn send(self, to: BareJid, resources: Vec<String>, id: String, path: PathBuf) {
        let peer = to.to_string();
        match self.transfer(&to, resources, &path).await {
            Ok((url, delivered)) => {
                self.net.emit(Ev::Uploaded { peer: peer.clone(), id: id.clone(), url });
                if delivered {
                    self.net.emit(Ev::Receipt { peer, id });
                }
            }
            Err(error) => self.net.emit(Ev::SendFailed { peer, id, error }),
        }
    }

    /// The URL and whether the receiver confirmed it got the whole file.
    async fn transfer(&self, to: &BareJid, resources: Vec<String>, path: &std::path::Path) -> Result<(String, bool), String> {
        let target = self.pick(to, resources).await.ok_or_else(|| t!("ft.no_client").to_string())?;
        let data = tokio::fs::read(path).await.map_err(|e| e.to_string())?;
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file").to_string();
        let (sid, ibb_sid) = (new_id(), new_id());
        let initiate = offer(&sid, &ibb_sid, &name, &data, self.bound.lock().unwrap().clone());
        let (accept_tx, accept_rx) = oneshot::channel();
        let (done_tx, done_rx) = oneshot::channel();
        self.sessions.lock().unwrap().outgoing.insert(sid.clone(), Outgoing { accept: Some(accept_tx), done: Some(done_tx) });
        let res = async {
            self.net.iq(Some(target.clone()), IqRequest::Set(initiate)).await?;
            let block = match tokio::time::timeout(ACCEPT_TIMEOUT, accept_rx).await {
                Ok(Ok(r)) => r?,
                Ok(Err(_)) => return Err(t!("ft.failed", reason = "gone").to_string()),
                Err(_) => {
                    self.terminate_out(&sid, target.clone(), "timeout");
                    return Err(t!("ft.no_answer").to_string());
                }
            };
            let ibb = |el: Element| IqRequest::Set(el);
            let open = Element::builder("open", NS_IBB).attr(nc("sid"), ibb_sid.as_str()).attr(nc("block-size"), block.to_string()).attr(nc("stanza"), "iq").build();
            self.net.iq(Some(target.clone()), ibb(open)).await?;
            futures::stream::iter(0..data.len().div_ceil(block))
                .map(|n| self.net.iq(Some(target.clone()), ibb(block_element(&ibb_sid, &data, block, n))))
                .buffered(WINDOW)
                .try_for_each(|_| async { Ok(()) })
                .await?;
            self.net.iq(Some(target.clone()), ibb(Element::builder("close", NS_IBB).attr(nc("sid"), ibb_sid.as_str()).build())).await?;
            // The receiver ends the session once it has checked the file.
            Ok(match tokio::time::timeout(Duration::from_secs(30), done_rx).await {
                Ok(Ok(success)) => success,
                _ => {
                    self.terminate_out(&sid, target.clone(), "success");
                    false
                }
            })
        }
        .await;
        if res.is_err() {
            self.terminate_out(&sid, target, "cancel");
        }
        self.sessions.lock().unwrap().outgoing.remove(&sid);
        res.map(|delivered| (url(&sid, &name), delivered))
    }

    fn terminate_out(&self, sid: &str, to: Jid, reason: &str) {
        if self.sessions.lock().unwrap().outgoing.remove(sid).is_some() {
            self.terminate_to(sid, to, reason);
        }
    }

    /// The first resource whose client takes Jingle files over IBB.
    async fn pick(&self, to: &BareJid, resources: Vec<String>) -> Option<Jid> {
        for r in resources {
            let Ok(full) = to.with_resource_str(&r) else { continue };
            let full = Jid::from(full);
            if let Ok(Some(el)) = self.net.iq(Some(full.clone()), IqRequest::Get(DiscoInfoQuery { node: None }.into())).await
                && let Ok(info) = DiscoInfoResult::try_from(el)
                && [ns::JINGLE, NS_FT, NS_JINGLE_IBB].iter().all(|f| info.features.iter().any(|x| x == f))
            {
                return Some(full);
            }
        }
        None
    }
}

/// Our session-initiate for `data`, with its SHA-256 up front (XEP-0234 §5).
fn offer(sid: &str, ibb_sid: &str, name: &str, data: &[u8], initiator: Option<Jid>) -> Element {
    let hash = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(data));
    let file = Element::builder("file", NS_FT)
        .append(Element::builder("name", NS_FT).append(name))
        .append(Element::builder("size", NS_FT).append(data.len().to_string()))
        .append(Element::builder("hash", NS_HASHES).attr(nc("algo"), "sha-256").append(hash));
    let content = Element::builder("content", ns::JINGLE)
        .attr(nc("creator"), "initiator")
        .attr(nc("name"), "file")
        .attr(nc("senders"), "initiator")
        .append(Element::builder("description", NS_FT).append(file))
        .append(Element::builder("transport", NS_JINGLE_IBB).attr(nc("sid"), ibb_sid).attr(nc("block-size"), BLOCK.to_string()));
    let mut el = jingle("session-initiate", sid).append(content);
    if let Some(me) = initiator {
        el = el.attr(nc("initiator"), me.to_string());
    }
    el.build()
}

/// IBB block `n` of `data`; the sequence wraps at 65536 (XEP-0047 §2.2).
fn block_element(ibb_sid: &str, data: &[u8], block: usize, n: usize) -> Element {
    let chunk = &data[n * block..((n + 1) * block).min(data.len())];
    Element::builder("data", NS_IBB)
        .attr(nc("sid"), ibb_sid)
        .attr(nc("seq"), (n % 65536).to_string())
        .append(base64::engine::general_purpose::STANDARD.encode(chunk))
        .build()
}

fn jingle(action: &str, sid: &str) -> tokio_xmpp::minidom::ElementBuilder {
    Element::builder("jingle", ns::JINGLE).attr(nc("action"), action).attr(nc("sid"), sid)
}

/// The SHA-256 a `<file>` carries (XEP-0300), if any.
fn sha256_of(file: &Element) -> Option<Vec<u8>> {
    file.children()
        .find(|h| h.is("hash", NS_HASHES) && h.attr("algo") == Some("sha-256"))
        .and_then(|h| base64::engine::general_purpose::STANDARD.decode(h.text().trim()).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_carry_the_session() {
        let u = url("S1", "a/b?c#d.png");
        assert_eq!(u, "jingle://S1/a_b_c_d.png");
        assert_eq!(sid_of(&u), Some("S1"));
        assert_eq!(sid_of("https://x/y"), None);
    }

    /// A Conversations-style offer: `<mediaType>`, no hash, block size 8192; then IBB data with a
    /// sequence that does not wrap, a checksum, and the close.
    #[test]
    fn receives_a_file() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let (net, mut ev, _out) = Net::for_tests_with_events();
            let ft = FileTransfers::new(net);
            let from: Jid = "bob@x/phone".parse().unwrap();
            let offer: Element = "<jingle xmlns='urn:xmpp:jingle:1' action='session-initiate' sid='S'>\
                <content creator='initiator' name='f' senders='initiator'>\
                <description xmlns='urn:xmpp:jingle:apps:file-transfer:5'><file><mediaType>text/plain</mediaType><name>hi.txt</name><size>5</size></file></description>\
                <transport xmlns='urn:xmpp:jingle:transports:ibb:1' sid='I' block-size='8192'/></content></jingle>"
                .parse()
                .unwrap();
            assert!(ft.owns(&offer));
            ft.on_jingle(Some(from.clone()), offer).unwrap();
            let Some(Ev::Message { peer, msg, .. }) = ev.next().await else { panic!("no offer message") };
            assert_eq!((peer.as_str(), msg.url.as_deref()), ("bob@x", Some("jingle://S/hi.txt")));

            let path = std::env::temp_dir().join(format!("crabber-ft-{}.txt", std::process::id()));
            ft.accept("jingle://S/hi.txt", path.clone());
            let ibb = |s: &str| -> Element { s.parse().unwrap() };
            ft.on_ibb(Some(from.clone()), &ibb("<open xmlns='http://jabber.org/protocol/ibb' sid='I' block-size='8192' stanza='iq'/>")).unwrap();
            assert_eq!(ft.on_ibb(Some("eve@x/pc".parse().unwrap()), &ibb("<data xmlns='http://jabber.org/protocol/ibb' sid='I' seq='0'>aGk=</data>")), Err(DefinedCondition::ItemNotFound), "only the offerer");
            ft.on_ibb(Some(from.clone()), &ibb("<data xmlns='http://jabber.org/protocol/ibb' sid='I' seq='0'>aGVs</data>")).unwrap();
            ft.on_ibb(Some(from.clone()), &ibb("<data xmlns='http://jabber.org/protocol/ibb' sid='I' seq='1'>bG8=</data>")).unwrap();
            let checksum: Element = format!(
                "<jingle xmlns='urn:xmpp:jingle:1' action='session-info' sid='S'><checksum xmlns='urn:xmpp:jingle:apps:file-transfer:5' creator='initiator' name='f'><file><hash xmlns='urn:xmpp:hashes:2' algo='sha-256'>{}</hash></file></checksum></jingle>",
                base64::engine::general_purpose::STANDARD.encode(Sha256::digest(b"hello"))
            )
            .parse()
            .unwrap();
            ft.on_jingle(Some(from.clone()), checksum).unwrap();
            ft.on_ibb(Some(from.clone()), &ibb("<close xmlns='http://jabber.org/protocol/ibb' sid='I'/>")).unwrap();
            match ev.next().await {
                Some(Ev::Downloaded { url }) => assert_eq!(url, "jingle://S/hi.txt"),
                other => panic!("{other:?}"),
            }
            assert_eq!(std::fs::read(&path).unwrap(), b"hello");
            let _ = std::fs::remove_file(path);
        });
    }

    /// What our sender puts on the wire is what our receiver takes: offer, blocks, checksum.
    #[test]
    fn our_sender_and_receiver_agree() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let (net, mut ev, _out) = Net::for_tests_with_events();
            let ft = FileTransfers::new(net);
            let from: Jid = "alice@x/pc".parse().unwrap();
            // Three blocks, the last one short.
            let data: Vec<u8> = (0..BLOCK * 2 + 100).map(|i| (i % 251) as u8).collect();
            let el = offer("S", "I", "файл.bin", &data, Some(from.clone()));
            assert_eq!(el.attr("initiator"), Some("alice@x/pc"));
            ft.on_jingle(Some(from.clone()), el).unwrap();
            let Some(Ev::Message { msg, .. }) = ev.next().await else { panic!("no offer") };
            let url = msg.url.unwrap();
            let path = std::env::temp_dir().join(format!("crabber-ft-rt-{}", std::process::id()));
            ft.accept(&url, path.clone());
            for n in 0..data.len().div_ceil(BLOCK) {
                ft.on_ibb(Some(from.clone()), &block_element("I", &data, BLOCK, n)).unwrap();
            }
            ft.on_ibb(Some(from), &Element::builder("close", NS_IBB).attr(nc("sid"), "I").build()).unwrap();
            match ev.next().await {
                Some(Ev::Downloaded { url: u }) => assert_eq!(u, url),
                other => panic!("{other:?}"),
            }
            assert_eq!(std::fs::read(&path).unwrap(), data);
            let _ = std::fs::remove_file(path);
        });
        assert_eq!(block_element("I", &[0; 65538], 1, 65537).attr("seq"), Some("1"), "the sequence wraps");
    }

    #[test]
    fn a_wrong_checksum_fails() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let (net, mut ev, _out) = Net::for_tests_with_events();
            let ft = FileTransfers::new(net);
            let from: Jid = "bob@x/pc".parse().unwrap();
            let offer: Element = "<jingle xmlns='urn:xmpp:jingle:1' action='session-initiate' sid='S'>\
                <content creator='initiator' name='f' senders='initiator'>\
                <description xmlns='urn:xmpp:jingle:apps:file-transfer:5'><file><name>a.bin</name><size>2</size><hash xmlns='urn:xmpp:hashes:2' algo='sha-256'>AAAA</hash></file></description>\
                <transport xmlns='urn:xmpp:jingle:transports:ibb:1' sid='I'/></content></jingle>"
                .parse()
                .unwrap();
            ft.on_jingle(Some(from.clone()), offer).unwrap();
            let _ = ev.next().await;
            ft.accept("jingle://S/a.bin", std::env::temp_dir().join("crabber-ft-never"));
            let ibb = |s: &str| -> Element { s.parse().unwrap() };
            ft.on_ibb(Some(from.clone()), &ibb("<data xmlns='http://jabber.org/protocol/ibb' sid='I' seq='0'>aGk=</data>")).unwrap();
            ft.on_ibb(Some(from), &ibb("<close xmlns='http://jabber.org/protocol/ibb' sid='I'/>")).unwrap();
            assert!(matches!(ev.next().await, Some(Ev::DownloadFailed { .. })));
        });
    }
}
