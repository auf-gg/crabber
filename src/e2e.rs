//! End-to-end encryption manager on the network thread: OMEMO key storage, PEP publishing
//! (device list + bundle) and turning plaintext into `<encrypted/>` and back.
//!
//! Both OMEMO versions run side by side on one identity and device id: each has its own device
//! lists and sessions, trust is shared (keyed by the Curve25519 form of the identity key).
use crate::t;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, params};
use tokio_xmpp::IqRequest;
use tokio_xmpp::jid::Jid;
use tokio_xmpp::minidom::Element;

use crate::omemo::{self, Bundle, Identity, NODE_BUNDLES2, NODE_DEVICES, NODE_DEVICES2, NS, NS2, Session, b64, node_bundle, parse_pub, serialize_pub, unb64};
use crate::pgp;
use pgp::composed_types::{SignedPublicKey, SignedSecretKey};
use crate::xmpp::{Net, nc};

const NS_PUBSUB: &str = "http://jabber.org/protocol/pubsub";
pub const NS_PUBSUB_EVENT: &str = "http://jabber.org/protocol/pubsub#event";
const NS_DATA: &str = "jabber:x:data";
pub const NS_EME: &str = "urn:xmpp:eme:0";
const NS_SCE: &str = "urn:xmpp:sce:1";

pub type Shared = Arc<tokio::sync::Mutex<E2e>>;

/// Normal for archive copies made before this device existed; shown as a notice, not an error.
pub const NOT_FOR_THIS_DEVICE: &str = "not-for-this-device";

/// OMEMO version: legacy (`eu.siacs.conversations.axolotl`) or OMEMO 2 (`urn:xmpp:omemo:2`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Proto {
    Legacy,
    V2,
}

impl Proto {
    pub fn ns(self) -> &'static str {
        match self {
            Proto::Legacy => NS,
            Proto::V2 => NS2,
        }
    }

    pub fn of(el: &Element) -> Option<Proto> {
        [Proto::V2, Proto::Legacy].into_iter().find(|p| el.is("encrypted", p.ns()))
    }

    fn node_devices(self) -> &'static str {
        match self {
            Proto::Legacy => NODE_DEVICES,
            Proto::V2 => NODE_DEVICES2,
        }
    }

    /// Element holding the device list.
    fn list_name(self) -> &'static str {
        match self {
            Proto::Legacy => "list",
            Proto::V2 => "devices",
        }
    }

    /// PEP node of `device`'s bundle and its item id there (legacy: a node per device, any item).
    fn bundle_item(self, device: u32) -> (String, Option<String>) {
        match self {
            Proto::Legacy => (node_bundle(device), None),
            Proto::V2 => (NODE_BUNDLES2.into(), Some(device.to_string())),
        }
    }

    /// This version's table `name`: sessions and device lists are kept apart.
    fn table(self, name: &str) -> String {
        match self {
            Proto::Legacy => name.to_string(),
            Proto::V2 => format!("{name}2"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub id: u32,
    pub fingerprint: Option<String>,
    pub trusted: bool,
    pub own: bool,
    /// Listed for legacy OMEMO.
    pub legacy: bool,
    /// Listed for OMEMO 2.
    pub v2: bool,
}

pub struct E2e {
    db: Connection,
    pub me: Identity,
    pub my_jid: String,
    sessions: HashMap<(String, u32, Proto), Session>,
    devices: HashMap<(String, Proto), Vec<u32>>,
    /// Device lists already asked from the server (fetch or PEP notification) in this session.
    fresh: HashSet<(String, Proto)>,
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
             CREATE TABLE IF NOT EXISTS sessions2(jid TEXT, device INTEGER, state TEXT NOT NULL, PRIMARY KEY(jid, device));
             CREATE TABLE IF NOT EXISTS devices2(jid TEXT PRIMARY KEY, list TEXT NOT NULL);
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

    fn session(&mut self, jid: &str, device: u32, p: Proto) -> Option<Session> {
        let key = (jid.to_string(), device, p);
        if !self.sessions.contains_key(&key) {
            let sql = format!("SELECT state FROM {} WHERE jid=?1 AND device=?2", p.table("sessions"));
            let json: Option<String> = self.db.query_row(&sql, params![jid, device], |r| r.get(0)).optional().ok().flatten();
            let s = serde_json::from_str::<Session>(&json?).ok()?;
            self.sessions.insert(key.clone(), s);
        }
        self.sessions.get(&key).cloned()
    }

    fn put_session(&mut self, jid: &str, device: u32, p: Proto, s: Session) {
        let sql = format!("INSERT OR REPLACE INTO {}(jid, device, state) VALUES(?1, ?2, ?3)", p.table("sessions"));
        let _ = self.db.execute(&sql, params![jid, device, serde_json::to_string(&s).unwrap()]);
        self.sessions.insert((jid.to_string(), device, p), s);
    }

    pub fn devices(&mut self, jid: &str, p: Proto) -> Option<Vec<u32>> {
        let key = (jid.to_string(), p);
        if !self.devices.contains_key(&key) {
            let sql = format!("SELECT list FROM {} WHERE jid=?1", p.table("devices"));
            let json: Option<String> = self.db.query_row(&sql, [jid], |r| r.get(0)).optional().ok().flatten();
            self.devices.insert(key.clone(), serde_json::from_str(&json?).ok()?);
        }
        self.devices.get(&key).cloned()
    }

    pub fn set_devices(&mut self, jid: &str, p: Proto, list: Vec<u32>) {
        let sql = format!("INSERT OR REPLACE INTO {}(jid, list) VALUES(?1, ?2)", p.table("devices"));
        let _ = self.db.execute(&sql, params![jid, serde_json::to_string(&list).unwrap()]);
        self.devices.insert((jid.to_string(), p), list);
        self.fresh.insert((jid.to_string(), p));
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

    /// Devices of both OMEMO versions; one device id listed in both is one device.
    pub fn device_info(&mut self, jid: &str) -> Vec<DeviceInfo> {
        let own = jid == self.my_jid;
        let legacy = self.devices(jid, Proto::Legacy).unwrap_or_default();
        let v2 = self.devices(jid, Proto::V2).unwrap_or_default();
        let mut ids = legacy.clone();
        ids.extend(v2.iter().filter(|id| !legacy.contains(id)));
        ids.into_iter()
            .map(|id| {
                let (in_legacy, in_v2) = (legacy.contains(&id), v2.contains(&id));
                if own && id == self.me.device_id {
                    return DeviceInfo { id, fingerprint: Some(omemo::fingerprint(&self.me.identity.public)), trusted: true, own: true, legacy: in_legacy, v2: in_v2 };
                }
                let key: Option<Vec<u8>> = self.db.query_row("SELECT key FROM identities WHERE jid=?1 AND device=?2", params![jid, id], |r| r.get(0)).optional().ok().flatten();
                let fingerprint = key.and_then(|k| <[u8; 32]>::try_from(k.as_slice()).ok()).map(|k| omemo::fingerprint(&k));
                DeviceInfo { id, fingerprint, trusted: self.trusted(jid, id), own: false, legacy: in_legacy, v2: in_v2 }
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
    let child = |name: &str| el.get_child(name, NS).ok_or(t!("e2e.bundle_missing", name = name));
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

/// OMEMO 2 bundle: raw 32-byte keys, Ed25519 identity.
pub fn bundle2_element(b: &Bundle) -> Element {
    let mut prekeys = Element::builder("prekeys", NS2);
    for (id, k) in &b.prekeys {
        prekeys = prekeys.append(Element::builder("pk", NS2).attr(nc("id"), id.to_string()).append(b64(k)));
    }
    Element::builder("bundle", NS2)
        .append(Element::builder("spk", NS2).attr(nc("id"), b.signed_prekey_id.to_string()).append(b64(&b.signed_prekey)))
        .append(Element::builder("spks", NS2).append(b64(&b.signature)))
        .append(Element::builder("ik", NS2).append(b64(&b.identity)))
        .append(prekeys)
        .build()
}

pub fn parse_bundle2(el: &Element) -> Result<Bundle, String> {
    let child = |name: &str| el.get_child(name, NS2).ok_or(t!("e2e.bundle_missing", name = name));
    let key = |el: &Element| -> Result<[u8; 32], String> { unb64(&el.text())?.try_into().map_err(|_| t!("omemo.bad_pubkey").to_string()) };
    let spk = child("spk")?;
    let prekeys = child("prekeys")?.children().filter(|c| c.is("pk", NS2)).filter_map(|c| Some((c.attr("id")?.parse().ok()?, key(c).ok()?))).collect();
    Ok(Bundle {
        identity: key(child("ik")?)?,
        signed_prekey_id: spk.attr("id").and_then(|v| v.parse().ok()).unwrap_or(0),
        signed_prekey: key(spk)?,
        signature: unb64(&child("spks")?.text())?,
        prekeys,
    })
}

pub fn parse_devicelist(p: Proto, list: &Element) -> Vec<u32> {
    list.children().filter(|c| c.is("device", p.ns())).filter_map(|c| c.attr("id")?.parse().ok()).collect()
}

/// `list` (or a new one) with `id` added; the other devices stay as published, labels included.
fn with_device(p: Proto, list: Option<Element>, id: u32) -> Element {
    let mut list = list.unwrap_or_else(|| Element::builder(p.list_name(), p.ns()).build());
    list.append_child(Element::builder("device", p.ns()).attr(nc("id"), id.to_string()).build());
    list
}

/// Version and list element of a PEP event carrying a device list, if this message is one.
pub fn devicelist_event(payloads: &[Element]) -> Option<(Proto, Element)> {
    let items = payloads.iter().find(|p| p.is("event", NS_PUBSUB_EVENT))?.get_child("items", NS_PUBSUB_EVENT)?;
    let p = [Proto::Legacy, Proto::V2].into_iter().find(|p| items.attr("node") == Some(p.node_devices()))?;
    let list = items.children().find(|i| i.name() == "item")?.get_child(p.list_name(), p.ns())?;
    Some((p, list.clone()))
}

/// Publishes `item` as `id` on our PEP `node`, readable by anyone; `max_items`: keep every item
/// (OMEMO 2 keeps one bundle per device in one node).
async fn publish(net: &Net, node: &str, id: &str, item: Element, max_items: bool) -> Result<(), String> {
    let build = |with_options: bool| {
        let publish = Element::builder("publish", NS_PUBSUB).attr(nc("node"), node).append(Element::builder("item", NS_PUBSUB).attr(nc("id"), id).append(item.clone()));
        let mut pubsub = Element::builder("pubsub", NS_PUBSUB).append(publish);
        if with_options {
            let field = |var: &str, value: &str| Element::builder("field", NS_DATA).attr(nc("var"), var).append(Element::builder("value", NS_DATA).append(value));
            let mut x = Element::builder("x", NS_DATA)
                .attr(nc("type"), "submit")
                .append(field("FORM_TYPE", "http://jabber.org/protocol/pubsub#publish-options"))
                .append(field("pubsub#access_model", "open"));
            if max_items {
                x = x.append(field("pubsub#max_items", "max"));
            }
            pubsub = pubsub.append(Element::builder("publish-options", NS_PUBSUB).append(x));
        }
        pubsub.build()
    };
    // Bundles must be readable by non-contacts; fall back for servers that reject publish-options.
    match net.iq(None, IqRequest::Set(build(true))).await {
        Ok(_) => Ok(()),
        Err(_) => net.iq(None, IqRequest::Set(build(false))).await.map(|_| ()),
    }
}

/// Payload of item `id` of a PEP node, or of its newest item.
async fn fetch_item(net: &Net, jid: &str, node: &str, id: Option<&str>) -> Option<Element> {
    let to = Jid::from_str(jid).ok()?;
    let items = Element::builder("items", NS_PUBSUB).attr(nc("node"), node);
    let items = match id {
        Some(id) => items.append(Element::builder("item", NS_PUBSUB).attr(nc("id"), id)),
        None => items.attr(nc("max_items"), "1"),
    };
    let req = Element::builder("pubsub", NS_PUBSUB).append(items).build();
    let res = net.iq(Some(to), IqRequest::Get(req)).await.ok()??;
    res.get_child("items", NS_PUBSUB)?.children().find(|c| c.name() == "item")?.children().next().cloned()
}

/// Our bundle under both versions: they share the keys, so a used prekey leaves both.
async fn publish_bundles(net: &Net, me: &Identity) -> Result<(), String> {
    let legacy = publish(net, &node_bundle(me.device_id), "current", bundle_element(&me.bundle()), false).await;
    let v2 = publish(net, NODE_BUNDLES2, &me.device_id.to_string(), bundle2_element(&me.bundle2()), true).await;
    legacy.and(v2)
}

/// XEP-0420 envelope that OMEMO 2 encrypts: the body, random-length padding and our JID.
fn sce_envelope(body: &str, from: &str) -> Vec<u8> {
    let rpad = b64(&omemo::random::<32>());
    let rpad = &rpad[..1 + omemo::random::<1>()[0] as usize % rpad.len()];
    let envelope = Element::builder("envelope", NS_SCE)
        .append(Element::builder("content", NS_SCE).append(Element::builder("body", "jabber:client").append(body)))
        .append(Element::builder("rpad", NS_SCE).append(rpad))
        .append(Element::builder("from", NS_SCE).attr(nc("jid"), from))
        .build();
    let mut out = vec![];
    envelope.write_to(&mut out).expect("serialising to memory");
    out
}

/// Body of a decrypted SCE envelope; `None` when it carries none (nothing to show).
fn sce_body(xml: &[u8]) -> Result<Option<String>, String> {
    let envelope: Element = std::str::from_utf8(xml).map_err(|e| e.to_string())?.parse().map_err(|e: tokio_xmpp::minidom::Error| e.to_string())?;
    Ok(envelope.get_child("content", NS_SCE).and_then(|c| c.children().find(|c| c.name() == "body")).map(|b| b.text()))
}

// ---- flows ----

/// Publishes our bundles and makes sure our device id is in both device lists.
pub async fn on_online(shared: Shared, net: Net) {
    let (me, my_jid) = {
        let mut e = shared.lock().await;
        if e.me.refill_prekeys() {
            e.save_identity();
        }
        (e.me.clone(), e.my_jid.clone())
    };
    if let Err(err) = publish_bundles(&net, &me).await {
        net.emit(crate::xmpp::Ev::Error(t!("e2e.publish_failed", err = err)));
    }
    for p in [Proto::Legacy, Proto::V2] {
        let list = fetch_item(&net, &my_jid, p.node_devices(), None).await.filter(|l| l.is(p.list_name(), p.ns()));
        let mut ids = list.as_ref().map(|l| parse_devicelist(p, l)).unwrap_or_default();
        if !ids.contains(&me.device_id) {
            let list = with_device(p, list, me.device_id);
            ids = parse_devicelist(p, &list);
            let _ = publish(&net, p.node_devices(), "current", list, false).await;
        }
        shared.lock().await.set_devices(&my_jid, p, ids);
    }
}

/// A device list update (PEP notification). Re-adds our own id if another client dropped it.
pub async fn on_devicelist(shared: Shared, net: Net, jid: String, p: Proto, list: Element) {
    let ids = parse_devicelist(p, &list);
    let (my_jid, my_id) = {
        let mut e = shared.lock().await;
        e.set_devices(&jid, p, ids.clone());
        (e.my_jid.clone(), e.me.device_id)
    };
    if jid == my_jid && !ids.contains(&my_id) {
        let list = with_device(p, Some(list), my_id);
        let ids = parse_devicelist(p, &list);
        let _ = publish(&net, p.node_devices(), "current", list, false).await;
        shared.lock().await.set_devices(&jid, p, ids);
    }
}

/// Cached list, re-read from the server once per session: PEP notifications need a mutual
/// subscription, so without them a contact's new device would never show up.
async fn device_list(shared: &Shared, net: &Net, jid: &str, p: Proto) -> Vec<u32> {
    let key = (jid.to_string(), p);
    let cached = {
        let mut e = shared.lock().await;
        let cached = e.devices(jid, p).filter(|d| !d.is_empty());
        if e.fresh.contains(&key) && (cached.is_some() || p == Proto::V2) {
            return cached.unwrap_or_default();
        }
        cached
    };
    match fetch_item(net, jid, p.node_devices(), None).await.filter(|l| l.is(p.list_name(), p.ns())).map(|l| parse_devicelist(p, &l)).filter(|d| !d.is_empty()) {
        Some(ids) => {
            shared.lock().await.set_devices(jid, p, ids.clone());
            ids
        }
        // Fetch failed or no list: keep what we knew. OMEMO 2 is asked first for every message,
        // so its absence is remembered too, or each message to a legacy-only contact would wait.
        None => {
            if p == Proto::V2 {
                shared.lock().await.fresh.insert(key);
            }
            cached.unwrap_or_default()
        }
    }
}

/// Re-reads both device lists of `jid` from the server.
pub async fn refresh(shared: &Shared, net: &Net, jid: &str) {
    shared.lock().await.fresh.retain(|(j, _)| j != jid);
    for p in [Proto::Legacy, Proto::V2] {
        device_list(shared, net, jid, p).await;
    }
}

/// OMEMO version for a message to `jid` in chat mode `mode`: OMEMO 2 when any of the contact's
/// devices has it, else legacy; "omemo2" / "omemo-legacy" allow only that one. `None`: no devices.
pub async fn pick(shared: &Shared, net: &Net, jid: &str, mode: &str) -> Option<Proto> {
    let allowed: &[Proto] = match mode {
        "omemo2" => &[Proto::V2],
        "omemo-legacy" => &[Proto::Legacy],
        _ => &[Proto::V2, Proto::Legacy],
    };
    for &p in allowed {
        if !device_list(shared, net, jid, p).await.is_empty() {
            return Some(p);
        }
    }
    None
}

/// Encrypts `plaintext` with OMEMO version `p` for all trusted devices of `to` and our other devices.
pub async fn encrypt(shared: &Shared, net: &Net, to: &str, plaintext: &str, p: Proto) -> Result<Element, String> {
    let (my_jid, my_id) = {
        let e = shared.lock().await;
        (e.my_jid.clone(), e.me.device_id)
    };
    let theirs = device_list(shared, net, to, p).await;
    if theirs.is_empty() {
        return Err(t!("e2e.err_no_devices").into());
    }
    let mine: Vec<u32> = device_list(shared, net, &my_jid, p).await.into_iter().filter(|d| *d != my_id).collect();
    let targets: Vec<(String, u32)> = theirs.into_iter().map(|d| (to.to_string(), d)).chain(mine.into_iter().map(|d| (my_jid.clone(), d))).collect();

    // Build missing sessions (bundle fetches happen without holding the lock).
    for (jid, dev) in &targets {
        if shared.lock().await.session(jid, *dev, p).is_some() {
            continue;
        }
        let (node, item) = p.bundle_item(*dev);
        let Some(el) = fetch_item(net, jid, &node, item.as_deref()).await else { continue };
        let bundle = match p {
            Proto::Legacy => parse_bundle(&el),
            Proto::V2 => parse_bundle2(&el),
        };
        let Ok(bundle) = bundle else { continue };
        // Trust is kept for the Curve25519 form of the key, whichever version it came with.
        let identity = match p {
            Proto::Legacy => Ok(bundle.identity),
            Proto::V2 => omemo::ed25519_to_x25519(&bundle.identity),
        };
        let Ok(identity) = identity else { continue };
        let mut e = shared.lock().await;
        if !e.check_identity(jid, *dev, &identity) {
            continue;
        }
        let session = match p {
            Proto::Legacy => Session::initiate(&e.me, &bundle),
            Proto::V2 => Session::initiate2(&e.me, &bundle),
        };
        if let Ok(s) = session {
            e.put_session(jid, *dev, p, s);
        }
    }

    // Legacy encrypts the body with AES-GCM, OMEMO 2 an SCE envelope with AES-CBC + HMAC.
    let (ciphertext, key_material, iv) = match p {
        Proto::Legacy => {
            let payload = omemo::encrypt_payload(plaintext.as_bytes());
            (payload.ciphertext, payload.key_material.to_vec(), Some(Element::builder("iv", NS).append(b64(&payload.iv)).build()))
        }
        Proto::V2 => {
            let (ciphertext, key_material) = omemo::encrypt_payload2(&sce_envelope(plaintext, &my_jid));
            (ciphertext, key_material, None)
        }
    };
    let mut e = shared.lock().await;
    let mut keys: Vec<(&str, Element)> = vec![];
    for (jid, dev) in &targets {
        if !e.trusted(jid, *dev) {
            continue;
        }
        let Some(mut s) = e.session(jid, *dev, p) else { continue };
        let me = e.me.clone();
        let (data, kex) = s.encrypt(&me, &key_material);
        e.put_session(jid, *dev, p, s);
        let mut key = Element::builder("key", p.ns()).attr(nc("rid"), dev.to_string());
        if kex {
            key = key.attr(nc(if p == Proto::V2 { "kex" } else { "prekey" }), "true");
        }
        keys.push((jid, key.append(b64(&data)).build()));
    }
    if !keys.iter().any(|(jid, _)| *jid == to) {
        return Err(t!("e2e.err_no_trusted").into());
    }
    let mut header = Element::builder("header", p.ns()).attr(nc("sid"), my_id.to_string());
    match p {
        Proto::Legacy => header = header.append_all(keys.into_iter().map(|(_, k)| k)),
        // OMEMO 2 groups the keys by recipient JID.
        Proto::V2 => {
            let mut jids: Vec<&str> = keys.iter().map(|(j, _)| *j).collect();
            jids.dedup();
            for jid in jids {
                let theirs = keys.iter().filter(|(j, _)| *j == jid).map(|(_, k)| k.clone());
                header = header.append(Element::builder("keys", NS2).attr(nc("jid"), jid).append_all(theirs));
            }
        }
    }
    let header = header.append_all(iv);
    Ok(Element::builder("encrypted", p.ns()).append(header).append(Element::builder("payload", p.ns()).append(b64(&ciphertext))).build())
}

/// Decrypts an `<encrypted/>` of either version from `from` (bare JID). `Ok(None)`: key-transport element, nothing to show.
pub async fn decrypt(shared: &Shared, net: &Net, from: &str, enc: &Element) -> Result<Option<String>, String> {
    let p = Proto::of(enc).ok_or(t!("e2e.err_no_header"))?;
    let ns = p.ns();
    let header = enc.get_child("header", ns).ok_or(t!("e2e.err_no_header"))?;
    let sid: u32 = header.attr("sid").and_then(|v| v.parse().ok()).ok_or(t!("e2e.err_no_sid"))?;
    let iv = match p {
        Proto::Legacy => unb64(&header.get_child("iv", NS).ok_or(t!("e2e.err_no_iv"))?.text())?,
        Proto::V2 => vec![],
    };
    let mut e = shared.lock().await;
    let (my_id, my_jid) = (e.me.device_id, e.my_jid.clone());
    let for_me = |k: &&Element| k.is("key", ns) && k.attr("rid").and_then(|v| v.parse::<u32>().ok()) == Some(my_id);
    let key = match p {
        Proto::Legacy => header.children().find(for_me),
        // OMEMO 2 groups the keys by recipient: ours are under our JID.
        Proto::V2 => header.children().filter(|k| k.is("keys", ns) && k.attr("jid") == Some(my_jid.as_str())).flat_map(|k| k.children()).find(for_me),
    }
    .ok_or(NOT_FOR_THIS_DEVICE)?;
    let kex = matches!(key.attr(if p == Proto::V2 { "kex" } else { "prekey" }), Some("true" | "1"));
    let data = unb64(&key.text())?;
    let existing = e.session(from, sid, p);
    let me = e.me.clone();
    let d = match p {
        Proto::Legacy => omemo::decrypt_key(&me, existing.as_ref(), &data, kex)?,
        Proto::V2 => omemo::decrypt_key2(&me, existing.as_ref(), &data, kex)?,
    };
    if !e.check_identity(from, sid, &d.session.their_identity) {
        return Err(t!("e2e.err_key_changed").into());
    }
    e.put_session(from, sid, p, d.session);
    if let Some(id) = d.used_prekey {
        // Never reuse a one-time prekey: replace it and republish the bundles.
        e.me.prekeys.remove(&id);
        e.me.refill_prekeys();
        e.save_identity();
        let (me, net) = (e.me.clone(), net.clone());
        tokio::spawn(async move {
            let _ = publish_bundles(&net, &me).await;
        });
    }
    let Some(payload) = enc.get_child("payload", ns) else { return Ok(None) };
    let payload = unb64(&payload.text())?;
    match p {
        Proto::Legacy => Ok(Some(String::from_utf8_lossy(&omemo::decrypt_payload(&d.plaintext, &iv, &payload)?).into_owned())),
        Proto::V2 => sce_body(&omemo::decrypt_payload2(&d.plaintext, &payload)?),
    }
}

/// Human name of an EME namespace (XEP-0380) for "encrypted with X" placeholders.
pub fn eme_name(ns: &str) -> &str {
    match ns {
        "eu.siacs.conversations.axolotl" => "OMEMO",
        "urn:xmpp:omemo:1" | "urn:xmpp:omemo:2" => "OMEMO 2",
        "urn:xmpp:openpgp:0" => "OpenPGP for XMPP (OX)",
        "jabber:x:encrypted" => "OpenPGP",
        "urn:xmpp:otr:0" => "OTR",
        _ => t!("e2e.unknown_protocol"),
    }
}
