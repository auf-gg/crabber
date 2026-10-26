//! End-to-end encryption manager on the network thread: OMEMO key storage, PEP publishing
//! (device list + bundle) and turning plaintext into `<encrypted/>` and back.
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, params};
use tokio_xmpp::IqRequest;
use tokio_xmpp::jid::Jid;
use tokio_xmpp::minidom::Element;

use crate::omemo::{self, Bundle, Identity, NS, NODE_DEVICES, Session, b64, node_bundle, parse_pub, serialize_pub, unb64};
use crate::pgp;
use pgp::composed_types::{SignedPublicKey, SignedSecretKey};
use crate::xmpp::{Net, nc};

const NS_PUBSUB: &str = "http://jabber.org/protocol/pubsub";
pub const NS_PUBSUB_EVENT: &str = "http://jabber.org/protocol/pubsub#event";
const NS_DATA: &str = "jabber:x:data";
pub const NS_EME: &str = "urn:xmpp:eme:0";

pub type Shared = Arc<tokio::sync::Mutex<E2e>>;

/// Normal for archive copies made before this device existed; shown as a notice, not an error.
pub const NOT_FOR_THIS_DEVICE: &str = "not-for-this-device";

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub id: u32,
    pub fingerprint: Option<String>,
    pub trusted: bool,
    pub own: bool,
}

pub struct E2e {
    db: Connection,
    pub me: Identity,
    pub my_jid: String,
    sessions: HashMap<(String, u32), Session>,
    devices: HashMap<String, Vec<u32>>,
    /// Device lists already taken from the server (fetch or PEP notification) in this session.
    fresh: HashSet<String>,
    pub pgp_secret: Option<SignedSecretKey>,
    /// Blind Trust Before Verification (on by default).
    pub blind_trust: bool,
    /// Key ids seen in contacts' signed presence.
    pub pgp_hints: HashMap<String, String>,
}

impl E2e {
    pub fn open(dir: &Path, my_jid: &str) -> rusqlite::Result<Self> {
        let db = Connection::open(dir.join("omemo.db"))?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS kv(k TEXT PRIMARY KEY, v TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS sessions(jid TEXT, device INTEGER, state TEXT NOT NULL, PRIMARY KEY(jid, device));
             CREATE TABLE IF NOT EXISTS devices(jid TEXT PRIMARY KEY, list TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS identities(jid TEXT, device INTEGER, key BLOB NOT NULL, trusted INTEGER NOT NULL DEFAULT 1, PRIMARY KEY(jid, device));
             CREATE TABLE IF NOT EXISTS pgp_keys(jid TEXT PRIMARY KEY, armored TEXT NOT NULL);",
        )?;
        // Devices the user confirmed by hand; once a contact has one, BTBV stops for that contact.
        let _ = db.execute_batch("ALTER TABLE identities ADD COLUMN verified INTEGER NOT NULL DEFAULT 0");
        let stored: Option<String> = db.query_row("SELECT v FROM kv WHERE k='identity'", [], |r| r.get(0)).optional()?;
        let me = match stored.and_then(|s| serde_json::from_str::<Identity>(&s).ok()) {
            Some(me) => me,
            None => {
                let me = Identity::generate();
                db.execute("INSERT OR REPLACE INTO kv(k, v) VALUES('identity', ?1)", [serde_json::to_string(&me).unwrap()])?;
                me
            }
        };
        let pgp_secret = db
            .query_row("SELECT v FROM kv WHERE k='pgp_secret'", [], |r| r.get::<_, String>(0))
            .optional()?
            .and_then(|a| pgp::parse_secret(&a).ok());
        Ok(Self { db, me, my_jid: my_jid.to_string(), sessions: HashMap::new(), devices: HashMap::new(), fresh: HashSet::new(), pgp_secret, pgp_hints: HashMap::new(), blind_trust: true })
    }

    // ---- OpenPGP (XEP-0027) ----

    pub fn pgp_set_secret(&mut self, key: Option<SignedSecretKey>) {
        match &key {
            Some(k) => {
                let _ = self.db.execute("INSERT OR REPLACE INTO kv(k, v) VALUES('pgp_secret', ?1)", [pgp::armor_secret(k)]);
            }
            None => {
                let _ = self.db.execute("DELETE FROM kv WHERE k='pgp_secret'", []);
            }
        }
        self.pgp_secret = key;
    }

    pub fn pgp_contact(&self, jid: &str) -> Option<SignedPublicKey> {
        let armored: String = self.db.query_row("SELECT armored FROM pgp_keys WHERE jid=?1", [jid], |r| r.get(0)).optional().ok().flatten()?;
        pgp::parse_public(&armored).ok()
    }

    pub fn pgp_set_contact(&self, jid: &str, key: &SignedPublicKey) {
        let _ = self.db.execute("INSERT OR REPLACE INTO pgp_keys(jid, armored) VALUES(?1, ?2)", params![jid, pgp::armor_public(key)]);
    }

    fn save_identity(&self) {
        let _ = self.db.execute("INSERT OR REPLACE INTO kv(k, v) VALUES('identity', ?1)", [serde_json::to_string(&self.me).unwrap()]);
    }

    fn session(&mut self, jid: &str, device: u32) -> Option<Session> {
        let key = (jid.to_string(), device);
        if !self.sessions.contains_key(&key) {
            let json: Option<String> = self.db.query_row("SELECT state FROM sessions WHERE jid=?1 AND device=?2", params![jid, device], |r| r.get(0)).optional().ok().flatten();
            let s = serde_json::from_str::<Session>(&json?).ok()?;
            self.sessions.insert(key.clone(), s);
        }
        self.sessions.get(&key).cloned()
    }

    fn put_session(&mut self, jid: &str, device: u32, s: Session) {
        let _ = self.db.execute("INSERT OR REPLACE INTO sessions(jid, device, state) VALUES(?1, ?2, ?3)", params![jid, device, serde_json::to_string(&s).unwrap()]);
        self.sessions.insert((jid.to_string(), device), s);
    }

    pub fn devices(&mut self, jid: &str) -> Option<Vec<u32>> {
        if !self.devices.contains_key(jid) {
            let json: Option<String> = self.db.query_row("SELECT list FROM devices WHERE jid=?1", [jid], |r| r.get(0)).optional().ok().flatten();
            self.devices.insert(jid.to_string(), serde_json::from_str(&json?).ok()?);
        }
        self.devices.get(jid).cloned()
    }

    pub fn set_devices(&mut self, jid: &str, list: Vec<u32>) {
        let _ = self.db.execute("INSERT OR REPLACE INTO devices(jid, list) VALUES(?1, ?2)", params![jid, serde_json::to_string(&list).unwrap()]);
        self.devices.insert(jid.to_string(), list);
        self.fresh.insert(jid.to_string());
    }

    /// Blind Trust Before Verification (as in Conversations): new devices and changed keys are
    /// trusted automatically until the user verified some device of this contact by hand;
    /// after that, unknown keys wait for confirmation.
    fn check_identity(&self, jid: &str, device: u32, key: &[u8; 32]) -> bool {
        let verified_any: bool = self.db.query_row("SELECT EXISTS(SELECT 1 FROM identities WHERE jid=?1 AND verified=1)", [jid], |r| r.get(0)).unwrap_or(false);
        let auto = self.blind_trust && !verified_any;
        let row: Option<(Vec<u8>, bool)> = self
            .db
            .query_row("SELECT key, trusted FROM identities WHERE jid=?1 AND device=?2", params![jid, device], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()
            .ok()
            .flatten();
        match row {
            None => {
                let _ = self.db.execute("INSERT INTO identities(jid, device, key, trusted) VALUES(?1, ?2, ?3, ?4)", params![jid, device, key.to_vec(), auto]);
                auto
            }
            Some((stored, trusted)) if stored == key => trusted,
            Some(_) => {
                let _ = self.db.execute("UPDATE identities SET key=?3, trusted=?4, verified=0 WHERE jid=?1 AND device=?2", params![jid, device, key.to_vec(), auto]);
                auto
            }
        }
    }

    fn trusted(&self, jid: &str, device: u32) -> bool {
        self.db.query_row("SELECT trusted FROM identities WHERE jid=?1 AND device=?2", params![jid, device], |r| r.get(0)).optional().ok().flatten().unwrap_or(true)
    }

    /// A manual decision; turning trust on counts as verification of that device.
    pub fn set_trusted(&self, jid: &str, device: u32, trusted: bool) {
        let _ = self.db.execute("UPDATE identities SET trusted=?3, verified=?3 WHERE jid=?1 AND device=?2", params![jid, device, trusted]);
    }

    pub fn device_info(&mut self, jid: &str) -> Vec<DeviceInfo> {
        let own = jid == self.my_jid;
        let ids = self.devices(jid).unwrap_or_default();
        ids.into_iter()
            .map(|id| {
                if own && id == self.me.device_id {
                    return DeviceInfo { id, fingerprint: Some(omemo::fingerprint(&self.me.identity.public)), trusted: true, own: true };
                }
                let key: Option<Vec<u8>> = self.db.query_row("SELECT key FROM identities WHERE jid=?1 AND device=?2", params![jid, id], |r| r.get(0)).optional().ok().flatten();
                let fingerprint = key.and_then(|k| <[u8; 32]>::try_from(k.as_slice()).ok()).map(|k| omemo::fingerprint(&k));
                DeviceInfo { id, fingerprint, trusted: self.trusted(jid, id), own: false }
            })
            .collect()
    }
}

// ---- XML ----

pub fn bundle_element(b: &Bundle) -> Element {
    let mut prekeys = Element::builder("prekeys", NS);
    for (id, k) in &b.prekeys {
        prekeys = prekeys.append(Element::builder("preKeyPublic", NS).attr(nc("preKeyId"), id.to_string()).append(b64(&serialize_pub(k))));
    }
    Element::builder("bundle", NS)
        .append(Element::builder("signedPreKeyPublic", NS).attr(nc("signedPreKeyId"), b.signed_prekey_id.to_string()).append(b64(&serialize_pub(&b.signed_prekey))))
        .append(Element::builder("signedPreKeySignature", NS).append(b64(&b.signature)))
        .append(Element::builder("identityKey", NS).append(b64(&serialize_pub(&b.identity))))
        .append(prekeys)
        .build()
}

pub fn parse_bundle(el: &Element) -> Result<Bundle, String> {
    let child = |name: &str| el.get_child(name, NS).ok_or(format!("в бандле нет {name}"));
    let spk = child("signedPreKeyPublic")?;
    let prekeys = child("prekeys")?
        .children()
        .filter(|c| c.is("preKeyPublic", NS))
        .filter_map(|c| Some((c.attr("preKeyId")?.parse().ok()?, parse_pub(&unb64(&c.text()).ok()?).ok()?)))
        .collect();
    Ok(Bundle {
        identity: parse_pub(&unb64(&child("identityKey")?.text())?)?,
        signed_prekey_id: spk.attr("signedPreKeyId").and_then(|v| v.parse().ok()).unwrap_or(0),
        signed_prekey: parse_pub(&unb64(&spk.text())?)?,
        signature: unb64(&child("signedPreKeySignature")?.text())?,
        prekeys,
    })
}

fn devicelist_element(ids: &[u32]) -> Element {
    let mut list = Element::builder("list", NS);
    for id in ids {
        list = list.append(Element::builder("device", NS).attr(nc("id"), id.to_string()));
    }
    list.build()
}

pub fn parse_devicelist(list: &Element) -> Vec<u32> {
    list.children().filter(|c| c.is("device", NS)).filter_map(|c| c.attr("id")?.parse().ok()).collect()
}

/// `<items>` of a PEP event carrying a device list, if this message is one.
pub fn devicelist_event(payloads: &[Element]) -> Option<Vec<u32>> {
    let items = payloads.iter().find(|p| p.is("event", NS_PUBSUB_EVENT))?.get_child("items", NS_PUBSUB_EVENT)?;
    if items.attr("node") != Some(NODE_DEVICES) {
        return None;
    }
    let list = items.children().find(|i| i.name() == "item")?.get_child("list", NS)?;
    Some(parse_devicelist(list))
}

async fn publish(net: &Net, node: &str, item: Element) -> Result<(), String> {
    let build = |with_options: bool| {
        let publish = Element::builder("publish", NS_PUBSUB).attr(nc("node"), node).append(Element::builder("item", NS_PUBSUB).attr(nc("id"), "current").append(item.clone()));
        let mut pubsub = Element::builder("pubsub", NS_PUBSUB).append(publish);
        if with_options {
            let field = |var: &str, value: &str| Element::builder("field", NS_DATA).attr(nc("var"), var).append(Element::builder("value", NS_DATA).append(value));
            pubsub = pubsub.append(
                Element::builder("publish-options", NS_PUBSUB).append(
                    Element::builder("x", NS_DATA)
                        .attr(nc("type"), "submit")
                        .append(field("FORM_TYPE", "http://jabber.org/protocol/pubsub#publish-options"))
                        .append(field("pubsub#access_model", "open")),
                ),
            );
        }
        pubsub.build()
    };
    // Bundles must be readable by non-contacts; fall back for servers that reject publish-options.
    match net.iq(None, IqRequest::Set(build(true))).await {
        Ok(_) => Ok(()),
        Err(_) => net.iq(None, IqRequest::Set(build(false))).await.map(|_| ()),
    }
}

/// Payload of the first item of a PEP node.
async fn fetch_item(net: &Net, jid: &str, node: &str) -> Option<Element> {
    let to = Jid::from_str(jid).ok()?;
    let req = Element::builder("pubsub", NS_PUBSUB).append(Element::builder("items", NS_PUBSUB).attr(nc("node"), node).attr(nc("max_items"), "1")).build();
    let res = net.iq(Some(to), IqRequest::Get(req)).await.ok()??;
    res.get_child("items", NS_PUBSUB)?.children().find(|c| c.name() == "item")?.children().next().cloned()
}

// ---- flows ----

/// Publishes our bundle and makes sure our device id is in our device list.
pub async fn on_online(shared: Shared, net: Net) {
    let (bundle, my_jid, my_id) = {
        let mut e = shared.lock().await;
        if e.me.refill_prekeys() {
            e.save_identity();
        }
        (e.me.bundle(), e.my_jid.clone(), e.me.device_id)
    };
    if let Err(err) = publish(&net, &node_bundle(my_id), bundle_element(&bundle)).await {
        net.emit(crate::xmpp::Ev::Error(format!("OMEMO: не удалось опубликовать ключи: {err}")));
    }
    let mut ids = match fetch_item(&net, &my_jid, NODE_DEVICES).await {
        Some(list) => parse_devicelist(&list),
        None => vec![],
    };
    if !ids.contains(&my_id) {
        ids.push(my_id);
        let _ = publish(&net, NODE_DEVICES, devicelist_element(&ids)).await;
    }
    shared.lock().await.set_devices(&my_jid, ids);
}

/// A device list update (PEP notification). Re-adds our own id if another client dropped it.
pub async fn on_devicelist(shared: Shared, net: Net, jid: String, mut ids: Vec<u32>) {
    let (my_jid, my_id) = {
        let mut e = shared.lock().await;
        e.set_devices(&jid, ids.clone());
        (e.my_jid.clone(), e.me.device_id)
    };
    if jid == my_jid && !ids.contains(&my_id) {
        ids.push(my_id);
        let _ = publish(&net, NODE_DEVICES, devicelist_element(&ids)).await;
        shared.lock().await.set_devices(&jid, ids);
    }
}

/// Cached list, re-read from the server once per session: PEP notifications need a mutual
/// subscription, so without them a contact's new device would never show up.
async fn device_list(shared: &Shared, net: &Net, jid: &str) -> Vec<u32> {
    let cached = {
        let mut e = shared.lock().await;
        let cached = e.devices(jid).filter(|d| !d.is_empty());
        if e.fresh.contains(jid) && cached.is_some() {
            return cached.unwrap_or_default();
        }
        cached
    };
    match fetch_item(net, jid, NODE_DEVICES).await.map(|l| parse_devicelist(&l)).filter(|d| !d.is_empty()) {
        Some(ids) => {
            shared.lock().await.set_devices(jid, ids.clone());
            ids
        }
        // Fetch failed or no list: keep what we knew.
        None => cached.unwrap_or_default(),
    }
}

/// Does `jid` publish OMEMO devices? (used for "auto" encryption)
pub async fn has_devices(shared: &Shared, net: &Net, jid: &str) -> bool {
    !device_list(shared, net, jid).await.is_empty()
}

/// Encrypts `plaintext` for all trusted devices of `to` and our other devices.
pub async fn encrypt(shared: &Shared, net: &Net, to: &str, plaintext: &str) -> Result<Element, String> {
    let (my_jid, my_id) = {
        let e = shared.lock().await;
        (e.my_jid.clone(), e.me.device_id)
    };
    let theirs = device_list(shared, net, to).await;
    if theirs.is_empty() {
        return Err("у собеседника нет устройств с OMEMO".into());
    }
    let mine: Vec<u32> = device_list(shared, net, &my_jid).await.into_iter().filter(|d| *d != my_id).collect();
    let targets: Vec<(String, u32)> = theirs.into_iter().map(|d| (to.to_string(), d)).chain(mine.into_iter().map(|d| (my_jid.clone(), d))).collect();

    // Build missing sessions (bundle fetches happen without holding the lock).
    for (jid, dev) in &targets {
        if shared.lock().await.session(jid, *dev).is_some() {
            continue;
        }
        let Some(el) = fetch_item(net, jid, &node_bundle(*dev)).await else { continue };
        let Ok(bundle) = parse_bundle(&el) else { continue };
        let mut e = shared.lock().await;
        if !e.check_identity(jid, *dev, &bundle.identity) {
            continue;
        }
        if let Ok(s) = Session::initiate(&e.me, &bundle) {
            e.put_session(jid, *dev, s);
        }
    }

    let payload = omemo::encrypt_payload(plaintext.as_bytes());
    let mut e = shared.lock().await;
    let mut header = Element::builder("header", NS).attr(nc("sid"), my_id.to_string());
    let mut for_peer = 0;
    for (jid, dev) in &targets {
        if !e.trusted(jid, *dev) {
            continue;
        }
        let Some(mut s) = e.session(jid, *dev) else { continue };
        let me = e.me.clone();
        let (data, prekey) = s.encrypt(&me, &payload.key_material);
        e.put_session(jid, *dev, s);
        let mut key = Element::builder("key", NS).attr(nc("rid"), dev.to_string());
        if prekey {
            key = key.attr(nc("prekey"), "true");
        }
        header = header.append(key.append(b64(&data)));
        if jid == to {
            for_peer += 1;
        }
    }
    if for_peer == 0 {
        return Err("нет доверенных устройств собеседника с OMEMO".into());
    }
    header = header.append(Element::builder("iv", NS).append(b64(&payload.iv)));
    Ok(Element::builder("encrypted", NS).append(header).append(Element::builder("payload", NS).append(b64(&payload.ciphertext))).build())
}

/// Decrypts an `<encrypted/>` from `from` (bare JID). `Ok(None)`: key-transport element, nothing to show.
pub async fn decrypt(shared: &Shared, net: &Net, from: &str, enc: &Element) -> Result<Option<String>, String> {
    let header = enc.get_child("header", NS).ok_or("нет заголовка OMEMO")?;
    let sid: u32 = header.attr("sid").and_then(|v| v.parse().ok()).ok_or("нет id устройства отправителя")?;
    let iv = unb64(&header.get_child("iv", NS).ok_or("нет IV")?.text())?;
    let mut e = shared.lock().await;
    let my_id = e.me.device_id;
    let key = header
        .children()
        .find(|k| k.is("key", NS) && k.attr("rid").and_then(|v| v.parse::<u32>().ok()) == Some(my_id))
        .ok_or(NOT_FOR_THIS_DEVICE)?;
    let prekey = matches!(key.attr("prekey"), Some("true" | "1"));
    let data = unb64(&key.text())?;
    let existing = e.session(from, sid);
    let me = e.me.clone();
    let d = omemo::decrypt_key(&me, existing.as_ref(), &data, prekey)?;
    if !e.check_identity(from, sid, &d.session.their_identity) {
        return Err("ключ устройства собеседника изменился — подтвердите его в настройках шифрования чата".into());
    }
    e.put_session(from, sid, d.session);
    if let Some(id) = d.used_prekey {
        // Never reuse a one-time prekey: replace it and republish the bundle.
        e.me.prekeys.remove(&id);
        e.me.refill_prekeys();
        e.save_identity();
        let (bundle, dev) = (e.me.bundle(), e.me.device_id);
        let net = net.clone();
        tokio::spawn(async move {
            let _ = publish(&net, &node_bundle(dev), bundle_element(&bundle)).await;
        });
    }
    let Some(payload) = enc.get_child("payload", NS) else { return Ok(None) };
    let pt = omemo::decrypt_payload(&d.plaintext, &iv, &unb64(&payload.text())?)?;
    Ok(Some(String::from_utf8_lossy(&pt).into_owned()))
}

/// Human name of an EME namespace (XEP-0380) for "encrypted with X" placeholders.
pub fn eme_name(ns: &str) -> &str {
    match ns {
        "eu.siacs.conversations.axolotl" => "OMEMO",
        "urn:xmpp:omemo:1" | "urn:xmpp:omemo:2" => "OMEMO 2",
        "urn:xmpp:openpgp:0" => "OpenPGP for XMPP (OX)",
        "jabber:x:encrypted" => "OpenPGP",
        "urn:xmpp:otr:0" => "OTR",
        _ => "неизвестный протокол",
    }
}
