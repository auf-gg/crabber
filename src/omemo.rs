//! OMEMO legacy (XEP-0384 v0.3, `eu.siacs.conversations.axolotl`): the libsignal v3 protocol
//! (X3DH + Double Ratchet + SignalMessage wire format) and the OMEMO payload/key layer.
//! OMEMO 2 (XEP-0384 v0.8+, `urn:xmpp:omemo:2`) runs on the same keys: the identity key is
//! published in its Ed25519 form, and sessions use the textbook Double Ratchet with the
//! OMEMOMessage/OMEMOKeyExchange wire format.
//!
//! This module is pure crypto and state; `crate::xmpp` does PEP publishing and stanza plumbing.
//! Interop is checked against python-oldmemo and python-twomemo in `tests/omemo_interop.rs`.
use crate::t;
use std::collections::BTreeMap;

use aes_gcm::aead::{Aead, KeyInit};
use base64::Engine;
use curve25519_dalek::edwards::{CompressedEdwardsY, EdwardsPoint};
use curve25519_dalek::montgomery::MontgomeryPoint;
use curve25519_dalek::scalar::Scalar;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};

pub const NS: &str = "eu.siacs.conversations.axolotl";
pub const NODE_DEVICES: &str = "eu.siacs.conversations.axolotl.devicelist";
pub fn node_bundle(device: u32) -> String {
    format!("eu.siacs.conversations.axolotl.bundles:{device}")
}
pub const FALLBACK_BODY: &str =
    "I sent you an OMEMO encrypted message but your client doesn’t seem to support that. Find more information on https://conversations.im/omemo";

pub const NS2: &str = "urn:xmpp:omemo:2";
pub const NODE_DEVICES2: &str = "urn:xmpp:omemo:2:devices";
/// One item per device, the item id being the device id.
pub const NODE_BUNDLES2: &str = "urn:xmpp:omemo:2:bundles";

const VERSION: u8 = 0x33;
const MAX_SKIP: u32 = 2000;
const RK_INFO2: &[u8] = b"OMEMO Root Chain";
const MK_INFO2: &[u8] = b"OMEMO Message Key Material";

pub fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}
pub fn unb64(s: &str) -> Result<Vec<u8>, String> {
    let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    base64::engine::general_purpose::STANDARD.decode(clean).map_err(|e| e.to_string())
}

pub fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("OS random number generator failed");
    b
}

fn clamp(mut k: [u8; 32]) -> [u8; 32] {
    k[0] &= 248;
    k[31] &= 127;
    k[31] |= 64;
    k
}

/// Curve25519 key pair (Montgomery form, like libsignal's "djb" keys).
#[derive(Clone, Serialize, Deserialize, PartialEq, Debug)]
pub struct KeyPair {
    pub private: [u8; 32],
    pub public: [u8; 32],
}

impl KeyPair {
    pub fn generate() -> Self {
        Self::from_private(clamp(random()))
    }
    pub fn from_private(private: [u8; 32]) -> Self {
        let private = clamp(private);
        Self { private, public: x25519_dalek::x25519(private, x25519_dalek::X25519_BASEPOINT_BYTES) }
    }
}

pub fn dh(private: &[u8; 32], public: &[u8; 32]) -> [u8; 32] {
    x25519_dalek::x25519(*private, *public)
}

/// libsignal serialisation of a public key: 0x05 type byte + 32 bytes.
pub fn serialize_pub(public: &[u8; 32]) -> [u8; 33] {
    let mut out = [0u8; 33];
    out[0] = 0x05;
    out[1..].copy_from_slice(public);
    out
}

pub fn parse_pub(data: &[u8]) -> Result<[u8; 32], String> {
    match data {
        [0x05, rest @ ..] if rest.len() == 32 => Ok(rest.try_into().unwrap()),
        rest if rest.len() == 32 => Ok(rest.try_into().unwrap()),
        _ => Err(t!("omemo.bad_pubkey").into()),
    }
}

/// XEdDSA signature with a Curve25519 key (sign bit of A forced to 0, as in the XEdDSA spec).
pub fn xeddsa_sign(private: &[u8; 32], msg: &[u8]) -> [u8; 64] {
    let mut a = Scalar::from_bytes_mod_order(clamp(*private));
    let mut big_a = EdwardsPoint::mul_base(&a).compress();
    if big_a.as_bytes()[31] & 0x80 != 0 {
        a = -a;
        big_a = EdwardsPoint::mul_base(&a).compress();
    }
    let mut h = Sha512::new();
    let mut prefix = [0xFFu8; 32];
    prefix[0] = 0xFE;
    h.update(prefix);
    h.update(a.as_bytes());
    h.update(msg);
    h.update(random::<64>());
    let r = Scalar::from_bytes_mod_order_wide(&h.finalize().into());
    let big_r = EdwardsPoint::mul_base(&r).compress();
    let k = Scalar::from_bytes_mod_order_wide(&Sha512::new().chain_update(big_r.as_bytes()).chain_update(big_a.as_bytes()).chain_update(msg).finalize().into());
    let s = r + k * a;
    let mut sig = [0u8; 64];
    sig[..32].copy_from_slice(big_r.as_bytes());
    sig[32..].copy_from_slice(s.as_bytes());
    sig
}

/// Verifies XEdDSA and legacy libsignal signatures (the latter carry A's sign bit in sig[63]).
pub fn xeddsa_verify(public: &[u8; 32], msg: &[u8], sig: &[u8]) -> bool {
    let Ok(mut sig): Result<[u8; 64], _> = sig.try_into() else { return false };
    let sign = sig[63] >> 7;
    sig[63] &= 0x7F;
    let Some(big_a) = MontgomeryPoint(*public).to_edwards(sign) else { return false };
    ed25519_verify(&big_a.compress().to_bytes(), msg, &sig)
}

/// Plain Ed25519 (RFC 8032) verification, as OMEMO 2 signs: XEdDSA signatures pass it too.
pub fn ed25519_verify(public: &[u8; 32], msg: &[u8], sig: &[u8]) -> bool {
    let Ok(sig): Result<[u8; 64], _> = sig.try_into() else { return false };
    let Some(big_a) = CompressedEdwardsY(*public).decompress() else { return false };
    let Some(s) = Option::<Scalar>::from(Scalar::from_canonical_bytes(sig[32..].try_into().unwrap())) else { return false };
    let Some(big_r) = CompressedEdwardsY(sig[..32].try_into().unwrap()).decompress() else { return false };
    let k = Scalar::from_bytes_mod_order_wide(&Sha512::new().chain_update(&sig[..32]).chain_update(public).chain_update(msg).finalize().into());
    EdwardsPoint::mul_base(&s) == big_r + k * big_a
}

/// Ed25519 form of a Curve25519 key pair's public key, with the sign bit 0 that XEdDSA signs with.
pub fn ed25519_public(private: &[u8; 32]) -> [u8; 32] {
    let mut a = EdwardsPoint::mul_base(&Scalar::from_bytes_mod_order(clamp(*private))).compress().to_bytes();
    a[31] &= 0x7F;
    a
}

/// Curve25519 form of an Ed25519 public key (the birational map of RFC 7748).
pub fn ed25519_to_x25519(public: &[u8; 32]) -> Result<[u8; 32], String> {
    CompressedEdwardsY(*public).decompress().map(|p| p.to_montgomery().to_bytes()).ok_or_else(|| t!("omemo.bad_pubkey").into())
}

fn hkdf(salt: &[u8], ikm: &[u8], info: &[u8], len: usize) -> Vec<u8> {
    let mut out = vec![0u8; len];
    hkdf::Hkdf::<Sha256>::new(Some(salt), ikm).expand(info, &mut out).expect("hkdf length");
    out
}

fn hmac(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("hmac key");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

fn split64(v: &[u8]) -> ([u8; 32], [u8; 32]) {
    (v[..32].try_into().unwrap(), v[32..64].try_into().unwrap())
}

/// Root-key ratchet step ("createChain" in libsignal).
fn kdf_rk(root: &[u8; 32], dh_out: &[u8; 32], info: &[u8]) -> ([u8; 32], [u8; 32]) {
    split64(&hkdf(root, dh_out, info, 64))
}

struct MessageKeys {
    cipher: [u8; 32],
    mac: [u8; 32],
    iv: [u8; 16],
}

/// 80 bytes of HKDF output split into an AES key, an HMAC key and an IV.
fn derive_keys(ikm: &[u8], info: &[u8]) -> MessageKeys {
    let k = hkdf(&[0u8; 32], ikm, info, 80);
    MessageKeys { cipher: k[..32].try_into().unwrap(), mac: k[32..64].try_into().unwrap(), iv: k[64..80].try_into().unwrap() }
}

fn message_keys(chain_key: &[u8; 32], info: &[u8]) -> MessageKeys {
    derive_keys(&hmac(chain_key, &[&[0x01]]), info)
}

fn next_chain_key(chain_key: &[u8; 32]) -> [u8; 32] {
    hmac(chain_key, &[&[0x02]])
}

fn aes_cbc_encrypt(key: &[u8; 32], iv: &[u8; 16], pt: &[u8]) -> Vec<u8> {
    use cbc::cipher::{BlockModeEncrypt, KeyIvInit, block_padding::Pkcs7};
    cbc::Encryptor::<aes::Aes256>::new(key.into(), iv.into()).encrypt_padded_vec::<Pkcs7>(pt)
}

fn aes_cbc_decrypt(key: &[u8; 32], iv: &[u8; 16], ct: &[u8]) -> Result<Vec<u8>, String> {
    use cbc::cipher::{BlockModeDecrypt, KeyIvInit, block_padding::Pkcs7};
    cbc::Decryptor::<aes::Aes256>::new(key.into(), iv.into()).decrypt_padded_vec::<Pkcs7>(ct).map_err(|_| t!("omemo.bad_padding").to_string())
}

// ---- minimal protobuf (only what SignalMessage / PreKeySignalMessage use) ----

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}
fn put_uint(out: &mut Vec<u8>, field: u32, v: u64) {
    put_varint(out, (field as u64) << 3);
    put_varint(out, v);
}
fn put_bytes(out: &mut Vec<u8>, field: u32, v: &[u8]) {
    put_varint(out, ((field as u64) << 3) | 2);
    put_varint(out, v.len() as u64);
    out.extend_from_slice(v);
}

enum PbValue<'a> {
    Int(u64),
    Bytes(&'a [u8]),
}

fn parse_pb(mut data: &[u8]) -> Result<BTreeMap<u32, PbValue<'_>>, String> {
    fn varint(data: &mut &[u8]) -> Result<u64, String> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let (&b, rest) = data.split_first().ok_or(t!("omemo.truncated"))?;
            *data = rest;
            v |= ((b & 0x7F) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(t!("omemo.varint").into())
    }
    let mut map = BTreeMap::new();
    while !data.is_empty() {
        let tag = varint(&mut data)?;
        let (field, wire) = ((tag >> 3) as u32, tag & 7);
        match wire {
            0 => {
                let v = varint(&mut data)?;
                map.insert(field, PbValue::Int(v));
            }
            2 => {
                let len = varint(&mut data)? as usize;
                if len > data.len() {
                    return Err(t!("omemo.truncated").into());
                }
                let (v, rest) = data.split_at(len);
                data = rest;
                map.insert(field, PbValue::Bytes(v));
            }
            _ => return Err(t!("omemo.field_type").into()),
        }
    }
    Ok(map)
}

fn pb_int(m: &BTreeMap<u32, PbValue<'_>>, f: u32) -> Option<u64> {
    match m.get(&f) {
        Some(PbValue::Int(v)) => Some(*v),
        _ => None,
    }
}
fn pb_bytes<'a>(m: &BTreeMap<u32, PbValue<'a>>, f: u32) -> Option<&'a [u8]> {
    match m.get(&f) {
        Some(PbValue::Bytes(v)) => Some(v),
        _ => None,
    }
}

// ---- identity & bundle ----

/// Our long-term OMEMO identity on this device.
#[derive(Clone, Serialize, Deserialize)]
pub struct Identity {
    pub device_id: u32,
    pub registration_id: u32,
    pub identity: KeyPair,
    pub signed_prekey_id: u32,
    pub signed_prekey: KeyPair,
    pub signed_prekey_sig: Vec<u8>,
    pub prekeys: BTreeMap<u32, KeyPair>,
    pub next_prekey_id: u32,
}

impl Identity {
    pub fn generate() -> Self {
        let rand32 = |b: [u8; 4]| u32::from_le_bytes(b);
        let identity = KeyPair::generate();
        let signed_prekey = KeyPair::generate();
        let signed_prekey_sig = xeddsa_sign(&identity.private, &serialize_pub(&signed_prekey.public)).to_vec();
        let mut me = Self {
            device_id: rand32(random()) % 0x7FFF_FFFF + 1,
            registration_id: rand32(random()) % 16380 + 1,
            identity,
            signed_prekey_id: 1,
            signed_prekey,
            signed_prekey_sig,
            prekeys: BTreeMap::new(),
            next_prekey_id: 1,
        };
        me.refill_prekeys();
        me
    }

    /// Keeps 100 one-time prekeys available.
    pub fn refill_prekeys(&mut self) -> bool {
        let mut changed = false;
        while self.prekeys.len() < 100 {
            self.prekeys.insert(self.next_prekey_id, KeyPair::generate());
            self.next_prekey_id = self.next_prekey_id % 0xFF_FFFE + 1;
            changed = true;
        }
        changed
    }

    pub fn bundle(&self) -> Bundle {
        Bundle {
            identity: self.identity.public,
            signed_prekey_id: self.signed_prekey_id,
            signed_prekey: self.signed_prekey.public,
            signature: self.signed_prekey_sig.clone(),
            prekeys: self.prekeys.iter().map(|(id, k)| (*id, k.public)).collect(),
        }
    }

    /// The same keys for OMEMO 2: Ed25519 identity, the signed prekey signed as raw bytes.
    pub fn bundle2(&self) -> Bundle {
        Bundle {
            identity: ed25519_public(&self.identity.private),
            signature: xeddsa_sign(&self.identity.private, &self.signed_prekey.public).to_vec(),
            ..self.bundle()
        }
    }
}

/// Human-readable fingerprint of an identity key (as shown by Conversations: hex of the 32 bytes).
pub fn fingerprint(identity_public: &[u8; 32]) -> String {
    identity_public.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().chunks(4).map(|c| c.concat()).collect::<Vec<_>>().join(" ")
}

#[derive(Clone, Debug)]
pub struct Bundle {
    /// Curve25519 in legacy bundles, Ed25519 in OMEMO 2 ones.
    pub identity: [u8; 32],
    pub signed_prekey_id: u32,
    pub signed_prekey: [u8; 32],
    pub signature: Vec<u8>,
    pub prekeys: Vec<(u32, [u8; 32])>,
}

// ---- Double Ratchet session ----

#[derive(Clone, Serialize, Deserialize)]
struct ReceiverChain {
    their_ratchet: [u8; 32],
    chain_key: [u8; 32],
    index: u32,
}

#[derive(Clone, Serialize, Deserialize)]
struct PendingPreKey {
    prekey_id: Option<u32>,
    signed_prekey_id: u32,
    base_key: [u8; 32],
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Session {
    pub their_identity: [u8; 32],
    root_key: [u8; 32],
    our_ratchet: KeyPair,
    their_ratchet: [u8; 32],
    send_chain: [u8; 32],
    send_index: u32,
    previous_counter: u32,
    receivers: Vec<ReceiverChain>,
    /// (their ratchet key, counter) -> chain key that yields that message's keys.
    skipped: Vec<([u8; 32], u32, [u8; 32])>,
    pending: Option<PendingPreKey>,
    local_registration_id: u32,
    /// Alice's base key when this session was created from a PreKeySignalMessage (dedup).
    pub base_key: Option<[u8; 32]>,
    /// OMEMO 2: the X3DH associated data, initiator's || responder's Ed25519 identity key.
    /// Empty in a legacy session.
    #[serde(default)]
    ad: Vec<u8>,
}

impl Session {
    fn v2(&self) -> bool {
        !self.ad.is_empty()
    }

    fn rk_info(&self) -> &'static [u8] {
        if self.v2() { RK_INFO2 } else { b"WhisperRatchet" }
    }

    /// X3DH as initiator against a fetched bundle.
    pub fn initiate(me: &Identity, bundle: &Bundle) -> Result<Self, String> {
        if !xeddsa_verify(&bundle.identity, &serialize_pub(&bundle.signed_prekey), &bundle.signature) {
            return Err(t!("omemo.bad_signature").into());
        }
        let prekey = if bundle.prekeys.is_empty() { None } else { Some(bundle.prekeys[u32::from_le_bytes(random()) as usize % bundle.prekeys.len()]) };
        let base = KeyPair::generate();
        let mut secret = vec![0xFFu8; 32];
        secret.extend(dh(&me.identity.private, &bundle.signed_prekey));
        secret.extend(dh(&base.private, &bundle.identity));
        secret.extend(dh(&base.private, &bundle.signed_prekey));
        if let Some((_, opk)) = prekey {
            secret.extend(dh(&base.private, &opk));
        }
        let (root, chain) = split64(&hkdf(&[0u8; 32], &secret, b"WhisperText", 64));
        let our_ratchet = KeyPair::generate();
        let (root2, send_chain) = kdf_rk(&root, &dh(&our_ratchet.private, &bundle.signed_prekey), b"WhisperRatchet");
        Ok(Self {
            their_identity: bundle.identity,
            root_key: root2,
            our_ratchet,
            their_ratchet: bundle.signed_prekey,
            send_chain,
            send_index: 0,
            previous_counter: 0,
            receivers: vec![ReceiverChain { their_ratchet: bundle.signed_prekey, chain_key: chain, index: 0 }],
            skipped: vec![],
            pending: Some(PendingPreKey { prekey_id: prekey.map(|p| p.0), signed_prekey_id: bundle.signed_prekey_id, base_key: base.public }),
            local_registration_id: me.registration_id,
            base_key: None,
            ad: vec![],
        })
    }

    /// OMEMO 2 X3DH as initiator; `bundle` is an OMEMO 2 one (Ed25519 identity key).
    pub fn initiate2(me: &Identity, bundle: &Bundle) -> Result<Self, String> {
        if !ed25519_verify(&bundle.identity, &bundle.signed_prekey, &bundle.signature) {
            return Err(t!("omemo.bad_signature").into());
        }
        // The peer rejects key exchanges without a one-time prekey.
        if bundle.prekeys.is_empty() {
            return Err(t!("omemo.no_prekeys").into());
        }
        let (prekey_id, opk) = bundle.prekeys[u32::from_le_bytes(random()) as usize % bundle.prekeys.len()];
        let their_identity = ed25519_to_x25519(&bundle.identity)?;
        let base = KeyPair::generate();
        let mut secret = vec![0xFFu8; 32];
        secret.extend(dh(&me.identity.private, &bundle.signed_prekey));
        secret.extend(dh(&base.private, &their_identity));
        secret.extend(dh(&base.private, &bundle.signed_prekey));
        secret.extend(dh(&base.private, &opk));
        let root: [u8; 32] = hkdf(&[0u8; 32], &secret, b"OMEMO X3DH", 32).try_into().unwrap();
        // Textbook Double Ratchet: no receiving chain until the peer answers with its own ratchet key.
        let our_ratchet = KeyPair::generate();
        let (root_key, send_chain) = kdf_rk(&root, &dh(&our_ratchet.private, &bundle.signed_prekey), RK_INFO2);
        Ok(Self {
            their_identity,
            root_key,
            our_ratchet,
            their_ratchet: bundle.signed_prekey,
            send_chain,
            send_index: 0,
            previous_counter: 0,
            receivers: vec![],
            skipped: vec![],
            pending: Some(PendingPreKey { prekey_id: Some(prekey_id), signed_prekey_id: bundle.signed_prekey_id, base_key: base.public }),
            local_registration_id: me.registration_id,
            base_key: None,
            ad: [ed25519_public(&me.identity.private), bundle.identity].concat(),
        })
    }

    /// OMEMO 2 X3DH as responder, from the fields of an OMEMOKeyExchange.
    fn respond2(me: &Identity, their_ik: [u8; 32], base_key: [u8; 32], prekey: &KeyPair) -> Result<Self, String> {
        let their_identity = ed25519_to_x25519(&their_ik)?;
        let mut secret = vec![0xFFu8; 32];
        secret.extend(dh(&me.signed_prekey.private, &their_identity));
        secret.extend(dh(&me.identity.private, &base_key));
        secret.extend(dh(&me.signed_prekey.private, &base_key));
        secret.extend(dh(&prekey.private, &base_key));
        Ok(Self {
            their_identity,
            root_key: hkdf(&[0u8; 32], &secret, b"OMEMO X3DH", 32).try_into().unwrap(),
            our_ratchet: me.signed_prekey.clone(),
            their_ratchet: [0u8; 32],
            // Set by the ratchet step the first message makes.
            send_chain: [0u8; 32],
            send_index: 0,
            previous_counter: 0,
            receivers: vec![],
            skipped: vec![],
            pending: None,
            local_registration_id: me.registration_id,
            base_key: Some(base_key),
            ad: [their_ik, ed25519_public(&me.identity.private)].concat(),
        })
    }

    /// X3DH as responder, from the fields of a PreKeySignalMessage.
    fn respond(me: &Identity, their_identity: [u8; 32], base_key: [u8; 32], prekey: Option<&KeyPair>) -> Self {
        let mut secret = vec![0xFFu8; 32];
        secret.extend(dh(&me.signed_prekey.private, &their_identity));
        secret.extend(dh(&me.identity.private, &base_key));
        secret.extend(dh(&me.signed_prekey.private, &base_key));
        if let Some(opk) = prekey {
            secret.extend(dh(&opk.private, &base_key));
        }
        let (root, chain) = split64(&hkdf(&[0u8; 32], &secret, b"WhisperText", 64));
        Self {
            their_identity,
            root_key: root,
            our_ratchet: me.signed_prekey.clone(),
            their_ratchet: [0u8; 32],
            send_chain: chain,
            send_index: 0,
            previous_counter: 0,
            receivers: vec![],
            skipped: vec![],
            pending: None,
            local_registration_id: me.registration_id,
            base_key: Some(base_key),
            ad: vec![],
        }
    }

    pub fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Encrypts with the sending chain; returns (wire bytes, is PreKeySignalMessage / OMEMOKeyExchange).
    pub fn encrypt(&mut self, me: &Identity, plaintext: &[u8]) -> (Vec<u8>, bool) {
        if self.v2() {
            return self.encrypt2(me, plaintext);
        }
        let keys = message_keys(&self.send_chain, b"WhisperMessageKeys");
        let ct = aes_cbc_encrypt(&keys.cipher, &keys.iv, plaintext);
        let mut body = vec![VERSION];
        put_bytes(&mut body, 1, &serialize_pub(&self.our_ratchet.public));
        put_uint(&mut body, 2, self.send_index as u64);
        put_uint(&mut body, 3, self.previous_counter as u64);
        put_bytes(&mut body, 4, &ct);
        let mac = hmac(&keys.mac, &[&serialize_pub(&me.identity.public), &serialize_pub(&self.their_identity), &body]);
        body.extend_from_slice(&mac[..8]);
        self.send_chain = next_chain_key(&self.send_chain);
        self.send_index += 1;
        match &self.pending {
            None => (body, false),
            Some(p) => {
                let mut out = vec![VERSION];
                put_uint(&mut out, 5, self.local_registration_id as u64);
                if let Some(id) = p.prekey_id {
                    put_uint(&mut out, 1, id as u64);
                }
                put_uint(&mut out, 6, p.signed_prekey_id as u64);
                put_bytes(&mut out, 2, &serialize_pub(&p.base_key));
                put_bytes(&mut out, 3, &serialize_pub(&me.identity.public));
                put_bytes(&mut out, 4, &body);
                (out, true)
            }
        }
    }

    /// OMEMO 2: an OMEMOAuthenticatedMessage, wrapped in an OMEMOKeyExchange until the peer answers.
    fn encrypt2(&mut self, me: &Identity, plaintext: &[u8]) -> (Vec<u8>, bool) {
        let keys = message_keys(&self.send_chain, MK_INFO2);
        // Serialised once: the MAC covers exactly these bytes.
        let mut msg = vec![];
        put_uint(&mut msg, 1, self.send_index as u64);
        put_uint(&mut msg, 2, self.previous_counter as u64);
        put_bytes(&mut msg, 3, &self.our_ratchet.public);
        put_bytes(&mut msg, 4, &aes_cbc_encrypt(&keys.cipher, &keys.iv, plaintext));
        let mac = hmac(&keys.mac, &[&self.ad, &msg]);
        let mut auth = vec![];
        put_bytes(&mut auth, 1, &mac[..16]);
        put_bytes(&mut auth, 2, &msg);
        self.send_chain = next_chain_key(&self.send_chain);
        self.send_index += 1;
        match &self.pending {
            None => (auth, false),
            Some(p) => {
                let mut out = vec![];
                put_uint(&mut out, 1, p.prekey_id.unwrap_or(0) as u64);
                put_uint(&mut out, 2, p.signed_prekey_id as u64);
                put_bytes(&mut out, 3, &ed25519_public(&me.identity.private));
                put_bytes(&mut out, 4, &p.base_key);
                put_bytes(&mut out, 5, &auth);
                (out, true)
            }
        }
    }

    /// OMEMO 2: decrypts an OMEMOAuthenticatedMessage (not the key exchange wrapper).
    fn decrypt2(&mut self, data: &[u8]) -> Result<Vec<u8>, String> {
        let auth = parse_pb(data)?;
        let mac = pb_bytes(&auth, 1).ok_or(t!("omemo.bad_mac"))?;
        let msg = pb_bytes(&auth, 2).ok_or(t!("omemo.no_ciphertext"))?;
        let pb = parse_pb(msg)?;
        let counter = pb_int(&pb, 1).unwrap_or(0) as u32;
        let their_ratchet = parse_pub(pb_bytes(&pb, 3).ok_or(t!("omemo.no_ratchet"))?)?;
        let ct = pb_bytes(&pb, 4).ok_or(t!("omemo.no_ciphertext"))?;

        // Work on a copy: a forged message must not advance the real state.
        let mut s = self.clone();
        let keys = message_keys(&s.chain_key_for(&their_ratchet, counter)?, MK_INFO2);
        if hmac(&keys.mac, &[&s.ad, msg])[..16] != *mac {
            return Err(t!("omemo.bad_mac").into());
        }
        let pt = aes_cbc_decrypt(&keys.cipher, &keys.iv, ct)?;
        s.pending = None; // the peer answered, so it has our session
        *self = s;
        Ok(pt)
    }

    /// Decrypts a SignalMessage / OMEMOAuthenticatedMessage (not the key exchange wrapper).
    fn decrypt_signal(&mut self, me: &Identity, data: &[u8]) -> Result<Vec<u8>, String> {
        if self.v2() {
            return self.decrypt2(data);
        }
        if data.len() < 1 + 8 || data[0] >> 4 != 3 {
            return Err(t!("omemo.bad_version").into());
        }
        let (body, mac) = data.split_at(data.len() - 8);
        let pb = parse_pb(&body[1..])?;
        let their_ratchet = parse_pub(pb_bytes(&pb, 1).ok_or(t!("omemo.no_ratchet"))?)?;
        let counter = pb_int(&pb, 2).unwrap_or(0) as u32;
        let ct = pb_bytes(&pb, 4).ok_or(t!("omemo.no_ciphertext"))?;

        // Work on a copy: a forged message must not advance the real state.
        let mut s = self.clone();
        let chain_key = s.chain_key_for(&their_ratchet, counter)?;
        let keys = message_keys(&chain_key, b"WhisperMessageKeys");
        let expect = hmac(&keys.mac, &[&serialize_pub(&s.their_identity), &serialize_pub(&me.identity.public), body]);
        if expect[..8] != *mac {
            return Err(t!("omemo.bad_mac").into());
        }
        let pt = aes_cbc_decrypt(&keys.cipher, &keys.iv, ct)?;
        s.pending = None; // the peer answered, so it has our session
        *self = s;
        Ok(pt)
    }

    /// Chain key for (ratchet, counter), ratcheting and storing skipped keys as needed.
    fn chain_key_for(&mut self, their_ratchet: &[u8; 32], counter: u32) -> Result<[u8; 32], String> {
        if let Some(pos) = self.skipped.iter().position(|(r, c, _)| r == their_ratchet && *c == counter) {
            return Ok(self.skipped.remove(pos).2);
        }
        if !self.receivers.iter().any(|c| c.their_ratchet == *their_ratchet) {
            // DH ratchet step.
            let (root, recv) = kdf_rk(&self.root_key, &dh(&self.our_ratchet.private, their_ratchet), self.rk_info());
            let our_new = KeyPair::generate();
            let (root2, send) = kdf_rk(&root, &dh(&our_new.private, their_ratchet), self.rk_info());
            self.root_key = root2;
            self.receivers.push(ReceiverChain { their_ratchet: *their_ratchet, chain_key: recv, index: 0 });
            if self.receivers.len() > 5 {
                self.receivers.remove(0);
            }
            // libsignal sends the last index of the previous chain, OMEMO 2 its length.
            self.previous_counter = if self.v2() { self.send_index } else { self.send_index.saturating_sub(1) };
            self.our_ratchet = our_new;
            self.their_ratchet = *their_ratchet;
            self.send_chain = send;
            self.send_index = 0;
        }
        let chain = self.receivers.iter_mut().find(|c| c.their_ratchet == *their_ratchet).unwrap();
        if counter < chain.index {
            return Err(t!("omemo.replay").into());
        }
        if counter - chain.index > MAX_SKIP {
            return Err(t!("omemo.too_many_skipped").into());
        }
        while chain.index < counter {
            self.skipped.push((chain.their_ratchet, chain.index, chain.chain_key));
            chain.chain_key = next_chain_key(&chain.chain_key);
            chain.index += 1;
        }
        let key = chain.chain_key;
        chain.chain_key = next_chain_key(&chain.chain_key);
        chain.index += 1;
        if self.skipped.len() > MAX_SKIP as usize {
            let excess = self.skipped.len() - MAX_SKIP as usize;
            self.skipped.drain(..excess);
        }
        Ok(key)
    }
}

/// Result of decrypting an OMEMO key element.
pub struct Decrypted {
    pub plaintext: Vec<u8>,
    pub session: Session,
    /// One-time prekey consumed: the bundle must be republished.
    pub used_prekey: Option<u32>,
}

/// Decrypts a `<key>`: SignalMessage with an existing session, or PreKeySignalMessage (new session).
pub fn decrypt_key(me: &Identity, existing: Option<&Session>, data: &[u8], prekey: bool) -> Result<Decrypted, String> {
    if !prekey {
        let mut s = existing.cloned().ok_or(t!("omemo.no_session"))?;
        let plaintext = s.decrypt_signal(me, data)?;
        return Ok(Decrypted { plaintext, session: s, used_prekey: None });
    }
    if data.first().is_none_or(|v| v >> 4 != 3) {
        return Err(t!("omemo.bad_prekey_version").into());
    }
    let pb = parse_pb(&data[1..])?;
    let base_key = parse_pub(pb_bytes(&pb, 2).ok_or(t!("omemo.no_base_key"))?)?;
    let their_identity = parse_pub(pb_bytes(&pb, 3).ok_or(t!("omemo.no_identity"))?)?;
    let inner = pb_bytes(&pb, 4).ok_or(t!("omemo.no_inner"))?;
    let prekey_id = pb_int(&pb, 1).map(|v| v as u32);
    let spk_id = pb_int(&pb, 6).unwrap_or(0) as u32;
    if spk_id != me.signed_prekey_id {
        return Err(t!("omemo.unknown_spk").into());
    }
    // A retransmitted initial message for a session we already built from it.
    if let Some(s) = existing.filter(|s| s.base_key == Some(base_key)) {
        let mut s = s.clone();
        let plaintext = s.decrypt_signal(me, inner)?;
        return Ok(Decrypted { plaintext, session: s, used_prekey: None });
    }
    let opk = match prekey_id {
        Some(id) => Some(me.prekeys.get(&id).ok_or(t!("omemo.prekey_used"))?),
        None => None,
    };
    let mut s = Session::respond(me, their_identity, base_key, opk);
    let plaintext = s.decrypt_signal(me, inner)?;
    Ok(Decrypted { plaintext, session: s, used_prekey: prekey_id })
}

/// OMEMO 2 `<key>`: an OMEMOAuthenticatedMessage with an existing session, or an
/// OMEMOKeyExchange (`kex`) that builds a new one.
pub fn decrypt_key2(me: &Identity, existing: Option<&Session>, data: &[u8], kex: bool) -> Result<Decrypted, String> {
    if !kex {
        let mut s = existing.cloned().ok_or(t!("omemo.no_session"))?;
        let plaintext = s.decrypt_signal(me, data)?;
        return Ok(Decrypted { plaintext, session: s, used_prekey: None });
    }
    let pb = parse_pb(data)?;
    let their_ik: [u8; 32] = pb_bytes(&pb, 3).and_then(|k| k.try_into().ok()).ok_or(t!("omemo.no_identity"))?;
    let base_key = parse_pub(pb_bytes(&pb, 4).ok_or(t!("omemo.no_base_key"))?)?;
    let inner = pb_bytes(&pb, 5).ok_or(t!("omemo.no_inner"))?;
    if pb_int(&pb, 2) != Some(me.signed_prekey_id as u64) {
        return Err(t!("omemo.unknown_spk").into());
    }
    // Sent again until we answer: the session is already built from it.
    if let Some(s) = existing.filter(|s| s.base_key == Some(base_key)) {
        let mut s = s.clone();
        let plaintext = s.decrypt_signal(me, inner)?;
        return Ok(Decrypted { plaintext, session: s, used_prekey: None });
    }
    let prekey_id = pb_int(&pb, 1).unwrap_or(0) as u32;
    let opk = me.prekeys.get(&prekey_id).ok_or(t!("omemo.prekey_used"))?;
    let mut s = Session::respond2(me, their_ik, base_key, opk)?;
    let plaintext = s.decrypt_signal(me, inner)?;
    Ok(Decrypted { plaintext, session: s, used_prekey: Some(prekey_id) })
}

// ---- OMEMO payload (AES-128-GCM, key||tag transported per device) ----

pub struct Payload {
    pub iv: [u8; 12],
    pub ciphertext: Vec<u8>,
    /// 16-byte key || 16-byte GCM tag, encrypted to each device.
    pub key_material: [u8; 32],
}

pub fn encrypt_payload(plaintext: &[u8]) -> Payload {
    let key: [u8; 16] = random();
    let iv: [u8; 12] = random();
    let cipher = aes_gcm::Aes128Gcm::new(&key.into());
    let mut out = cipher.encrypt(&iv.into(), plaintext).expect("gcm encrypt");
    let tag = out.split_off(out.len() - 16);
    let mut key_material = [0u8; 32];
    key_material[..16].copy_from_slice(&key);
    key_material[16..].copy_from_slice(&tag);
    Payload { iv, ciphertext: out, key_material }
}

pub fn decrypt_payload(key_material: &[u8], iv: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    // Old clients send a 16-byte key and append the tag to the payload.
    let (key, data) = match key_material.len() {
        32 => (&key_material[..16], [ciphertext, &key_material[16..]].concat()),
        16 => (key_material, ciphertext.to_vec()),
        _ => return Err(t!("omemo.bad_key_len").into()),
    };
    let iv: [u8; 12] = iv.try_into().map_err(|_| t!("omemo.iv_len").to_string())?;
    let key: [u8; 16] = key.try_into().unwrap();
    aes_gcm::Aes128Gcm::new(&key.into()).decrypt(&iv.into(), data.as_slice()).map_err(|_| t!("omemo.tampered").to_string())
}

// ---- OMEMO 2 payload (AES-256-CBC + HMAC-SHA-256, key||truncated HMAC transported per device) ----

/// Returns (ciphertext, the 48 bytes of key material to encrypt to each device).
pub fn encrypt_payload2(plaintext: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let key: [u8; 32] = random();
    let keys = derive_keys(&key, b"OMEMO Payload");
    let ct = aes_cbc_encrypt(&keys.cipher, &keys.iv, plaintext);
    let mac = hmac(&keys.mac, &[&ct]);
    (ct, [&key[..], &mac[..16]].concat())
}

pub fn decrypt_payload2(key_material: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, String> {
    if key_material.len() != 48 {
        return Err(t!("omemo.bad_key_len").into());
    }
    let (key, mac) = key_material.split_at(32);
    let keys = derive_keys(key, b"OMEMO Payload");
    if hmac(&keys.mac, &[ciphertext])[..16] != *mac {
        return Err(t!("omemo.tampered").into());
    }
    aes_cbc_decrypt(&keys.cipher, &keys.iv, ciphertext)
}

/// XEP-0454 file encryption: AES-256-GCM, URL fragment = hex(iv(12) || key(32)).
pub fn encrypt_file(data: &[u8]) -> (Vec<u8>, String) {
    let key: [u8; 32] = random();
    let iv: [u8; 12] = random();
    let ct = aes_gcm::Aes256Gcm::new(&key.into()).encrypt(&iv.into(), data).expect("gcm encrypt");
    let fragment: String = iv.iter().chain(key.iter()).map(|b| format!("{b:02x}")).collect();
    (ct, fragment)
}

pub fn decrypt_file(data: &[u8], fragment: &str) -> Result<Vec<u8>, String> {
    let bytes: Vec<u8> = (0..fragment.len() / 2).filter_map(|i| u8::from_str_radix(fragment.get(i * 2..i * 2 + 2)?, 16).ok()).collect();
    let (iv, key) = match bytes.len() {
        44 => (&bytes[..12], &bytes[12..]),
        48 => (&bytes[..16], &bytes[16..]),
        _ => return Err(t!("omemo.bad_file_key").into()),
    };
    if iv.len() != 12 {
        return Err(t!("omemo.iv_len").into());
    }
    let key: [u8; 32] = key.try_into().unwrap();
    let iv: [u8; 12] = iv.try_into().unwrap();
    aes_gcm::Aes256Gcm::new(&key.into()).decrypt(&iv.into(), data).map_err(|_| t!("omemo.file_corrupt").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xeddsa_roundtrip_and_tamper() {
        let k = KeyPair::generate();
        let sig = xeddsa_sign(&k.private, b"hello");
        assert!(xeddsa_verify(&k.public, b"hello", &sig));
        assert!(!xeddsa_verify(&k.public, b"hellp", &sig));
        let other = KeyPair::generate();
        assert!(!xeddsa_verify(&other.public, b"hello", &sig));
    }

    #[test]
    fn x3dh_and_double_ratchet_conversation() {
        let alice = Identity::generate();
        let mut bob = Identity::generate();
        let mut a = Session::initiate(&alice, &bob.bundle()).unwrap();

        // Two messages before Bob answers: both are PreKey messages.
        let (m1, pk1) = a.encrypt(&alice, b"one");
        let (m2, pk2) = a.encrypt(&alice, b"two");
        assert!(pk1 && pk2);
        let d1 = decrypt_key(&bob, None, &m1, true).unwrap();
        assert_eq!(d1.plaintext, b"one");
        bob.prekeys.remove(&d1.used_prekey.unwrap());
        let mut b = d1.session;
        // Second PreKey message reuses the same session (same base key), prekey already consumed.
        let d2 = decrypt_key(&bob, Some(&b), &m2, true).unwrap();
        assert_eq!(d2.plaintext, b"two");
        b = d2.session;

        // Bob answers; Alice leaves pending mode.
        let (r1, pk) = b.encrypt(&bob, b"hi alice");
        assert!(!pk);
        let d = decrypt_key(&alice, Some(&a), &r1, false).unwrap();
        assert_eq!(d.plaintext, b"hi alice");
        a = d.session;
        let (m3, pk3) = a.encrypt(&alice, b"three");
        assert!(!pk3, "no PreKey wrapper after the first reply");

        // Out-of-order delivery within a chain.
        let (r2, _) = b.encrypt(&bob, b"r2");
        let (r3, _) = b.encrypt(&bob, b"r3");
        let d3 = decrypt_key(&alice, Some(&a), &r3, false).unwrap();
        a = d3.session;
        let d2 = decrypt_key(&alice, Some(&a), &r2, false).unwrap();
        assert_eq!((d3.plaintext.as_slice(), d2.plaintext.as_slice()), (&b"r3"[..], &b"r2"[..]));
        a = d2.session;
        assert!(decrypt_key(&alice, Some(&a), &r2, false).is_err(), "replay is rejected");

        let d = decrypt_key(&bob, Some(&b), &m3, false).unwrap();
        assert_eq!(d.plaintext, b"three");

        // Tampering is detected and does not corrupt the session.
        let (mut bad, _) = d.session.clone().encrypt(&bob, b"x");
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(decrypt_key(&alice, Some(&a), &bad, false).is_err());
    }

    #[test]
    fn ed25519_form_of_identity() {
        let k = KeyPair::generate();
        let ed = ed25519_public(&k.private);
        assert_eq!(ed25519_to_x25519(&ed).unwrap(), k.public, "both forms are the same key");
        let sig = xeddsa_sign(&k.private, b"spk");
        assert!(ed25519_verify(&ed, b"spk", &sig), "XEdDSA signatures are plain Ed25519 ones");
        assert!(!ed25519_verify(&ed, b"spj", &sig));
        let me = Identity::generate();
        let b = me.bundle2();
        assert!(ed25519_verify(&b.identity, &b.signed_prekey, &b.signature));
    }

    #[test]
    fn omemo2_conversation() {
        let alice = Identity::generate();
        let mut bob = Identity::generate();
        let mut a = Session::initiate2(&alice, &bob.bundle2()).unwrap();
        assert_eq!(a.their_identity, bob.identity.public, "trust is keyed by the Curve25519 form");

        // Key exchanges until Bob answers; the second one reuses the session built from the first.
        let (m1, k1) = a.encrypt(&alice, b"one");
        let (m2, k2) = a.encrypt(&alice, b"two");
        assert!(k1 && k2);
        let d1 = decrypt_key2(&bob, None, &m1, true).unwrap();
        assert_eq!(d1.plaintext, b"one");
        assert_eq!(d1.session.their_identity, alice.identity.public);
        bob.prekeys.remove(&d1.used_prekey.unwrap());
        let mut b = d1.session;
        let d2 = decrypt_key2(&bob, Some(&b), &m2, true).unwrap();
        assert_eq!((d2.plaintext.as_slice(), d2.used_prekey), (&b"two"[..], None));
        b = d2.session;

        let (r1, kex) = b.encrypt(&bob, b"hi alice");
        assert!(!kex);
        let d = decrypt_key2(&alice, Some(&a), &r1, false).unwrap();
        assert_eq!(d.plaintext, b"hi alice");
        a = d.session;
        let (m3, k3) = a.encrypt(&alice, b"three");
        assert!(!k3, "no key exchange after the first reply");

        // Out of order, replay, tampering.
        let (r2, _) = b.encrypt(&bob, b"r2");
        let (r3, _) = b.encrypt(&bob, b"r3");
        a = decrypt_key2(&alice, Some(&a), &r3, false).unwrap().session;
        let d = decrypt_key2(&alice, Some(&a), &r2, false).unwrap();
        assert_eq!(d.plaintext, b"r2");
        a = d.session;
        assert!(decrypt_key2(&alice, Some(&a), &r2, false).is_err(), "replay is rejected");
        let d = decrypt_key2(&bob, Some(&b), &m3, false).unwrap();
        assert_eq!(d.plaintext, b"three");
        let (mut bad, _) = d.session.clone().encrypt(&bob, b"x");
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(decrypt_key2(&alice, Some(&a), &bad, false).is_err());

        // A legacy session can't read OMEMO 2 and vice versa.
        assert!(decrypt_key(&bob, None, &m1, true).is_err());
    }

    #[test]
    fn payload_and_file_encryption() {
        let p = encrypt_payload("привет".as_bytes());
        assert_eq!(decrypt_payload(&p.key_material, &p.iv, &p.ciphertext).unwrap(), "привет".as_bytes());
        let mut wrong = p.key_material;
        wrong[20] ^= 1;
        assert!(decrypt_payload(&wrong, &p.iv, &p.ciphertext).is_err());

        let (ct, km) = encrypt_payload2("привет".as_bytes());
        assert_eq!(km.len(), 48);
        assert_eq!(decrypt_payload2(&km, &ct).unwrap(), "привет".as_bytes());
        let mut wrong = km.clone();
        wrong[40] ^= 1;
        assert!(decrypt_payload2(&wrong, &ct).is_err());

        let (ct, frag) = encrypt_file(b"file data");
        assert_eq!(frag.len(), 88);
        assert_eq!(decrypt_file(&ct, &frag).unwrap(), b"file data");
    }
}
