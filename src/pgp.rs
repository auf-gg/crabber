//! OpenPGP for XEP-0027 (`jabber:x:encrypted` / `jabber:x:signed`) on top of rpgp.
//! XEP-0027 carries ASCII armor without the BEGIN/END lines and headers.
use pgp::composed::{
    ArmorOptions, Deserializable, DetachedSignature, EncryptionCaps, KeyType, Message, MessageBuilder, SecretKeyParamsBuilder, SignedPublicKey, SignedSecretKey,
    SubkeyParamsBuilder,
};
use pgp::crypto::ecc_curve::ECCCurve;
use pgp::crypto::hash::HashAlgorithm;
use pgp::crypto::sym::SymmetricKeyAlgorithm;
use pgp::types::{KeyDetails, KeyId, Password};
use rand::thread_rng;

/// Re-exported so other modules need not depend on rpgp paths.
pub mod composed_types {
    pub use pgp::composed::{SignedPublicKey, SignedSecretKey};
}

pub const NS_ENCRYPTED: &str = "jabber:x:encrypted";
pub const NS_SIGNED: &str = "jabber:x:signed";
pub const FALLBACK_BODY: &str = "This message is encrypted with OpenPGP (XEP-0027).";

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Ed25519 primary (certify + sign) with a Curve25519 encryption subkey, as GnuPG creates by default.
pub fn generate(user_id: &str) -> Result<SignedSecretKey, String> {
    let mut enc = SubkeyParamsBuilder::default();
    enc.key_type(KeyType::ECDH(ECCCurve::Curve25519Legacy)).can_sign(false).can_encrypt(EncryptionCaps::All).can_authenticate(false);
    let mut params = SecretKeyParamsBuilder::default();
    params
        .key_type(KeyType::Ed25519Legacy)
        .can_certify(true)
        .can_sign(true)
        .can_encrypt(EncryptionCaps::None)
        .primary_user_id(user_id.into())
        .subkeys(vec![enc.build().map_err(err)?]);
    params.build().map_err(err)?.generate(thread_rng()).map_err(err)
}

pub fn parse_secret(armored: &str) -> Result<SignedSecretKey, String> {
    let (key, _) = SignedSecretKey::from_string(armored).map_err(|e| format!("не удалось прочитать закрытый ключ: {e}"))?;
    key.verify_bindings().map_err(err)?;
    Ok(key)
}

pub fn parse_public(armored: &str) -> Result<SignedPublicKey, String> {
    let (key, _) = SignedPublicKey::from_string(armored).map_err(|e| format!("не удалось прочитать открытый ключ: {e}"))?;
    key.verify_bindings().map_err(err)?;
    Ok(key)
}

pub fn public_of(secret: &SignedSecretKey) -> SignedPublicKey {
    SignedPublicKey::from(secret.clone())
}

pub fn armor_public(key: &SignedPublicKey) -> String {
    key.to_armored_string(ArmorOptions::default()).unwrap_or_default()
}

pub fn armor_secret(key: &SignedSecretKey) -> String {
    key.to_armored_string(ArmorOptions::default()).unwrap_or_default()
}

/// "AAAA BBBB ..." upper-case hex fingerprint.
pub fn fingerprint(key: &impl KeyDetails) -> String {
    let hex: String = key.fingerprint().as_bytes().iter().map(|b| format!("{b:02X}")).collect();
    hex.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join(" ")
}

fn hex_id(id: &KeyId) -> String {
    id.as_ref().iter().map(|b| format!("{b:02X}")).collect()
}

pub fn key_ids(key: &SignedPublicKey) -> Vec<String> {
    std::iter::once(key.legacy_key_id()).chain(key.public_subkeys.iter().map(|k| k.legacy_key_id())).map(|id| hex_id(&id)).collect()
}

/// Strips armor framing for XEP-0027: BEGIN/END lines and armor headers.
pub fn strip_armor(armored: &str) -> String {
    let mut body = vec![];
    let mut in_body = false;
    for line in armored.lines() {
        let line = line.trim_end();
        if line.starts_with("-----BEGIN") {
            continue;
        }
        if line.starts_with("-----END") {
            break;
        }
        if !in_body {
            // Headers ("Version: ...") end at the first empty line.
            if line.is_empty() {
                in_body = true;
            } else if !line.contains(": ") {
                in_body = true;
                body.push(line.to_string());
            }
            continue;
        }
        body.push(line.to_string());
    }
    body.join("\n")
}

pub fn add_armor(kind: &str, body: &str) -> String {
    let body: String = body.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n");
    format!("-----BEGIN PGP {kind}-----\n\n{body}\n-----END PGP {kind}-----\n")
}

/// Encrypts to every encryption-capable (sub)key of each recipient.
pub fn encrypt(text: &str, recipients: &[&SignedPublicKey]) -> Result<String, String> {
    let mut builder = MessageBuilder::from_bytes("", text.as_bytes().to_vec()).seipd_v1(thread_rng(), SymmetricKeyAlgorithm::AES256);
    let mut any = false;
    for key in recipients {
        for sub in key.public_subkeys.iter().filter(|s| s.algorithm().can_encrypt()) {
            builder.encrypt_to_key(thread_rng(), sub).map_err(err)?;
            any = true;
        }
        if key.public_subkeys.iter().all(|s| !s.algorithm().can_encrypt()) && key.primary_key.algorithm().can_encrypt() {
            builder.encrypt_to_key(thread_rng(), &key.primary_key).map_err(err)?;
            any = true;
        }
    }
    if !any {
        return Err("в ключе собеседника нет подключа для шифрования".into());
    }
    let armored = builder.to_armored_string(thread_rng(), ArmorOptions::default()).map_err(err)?;
    Ok(strip_armor(&armored))
}

pub fn decrypt(payload: &str, secret: &SignedSecretKey) -> Result<String, String> {
    let armored = add_armor("MESSAGE", payload);
    let (msg, _) = Message::from_string(&armored).map_err(|e| format!("неверное PGP-сообщение: {e}"))?;
    let mut msg = msg.decrypt(&Password::empty(), secret).map_err(|e| format!("не удалось расшифровать: {e}"))?;
    if msg.is_compressed() {
        msg = msg.decompress().map_err(err)?;
    }
    let data = msg.as_data_vec().map_err(err)?;
    String::from_utf8(data).map_err(|_| "сообщение не в UTF-8".into())
}

/// Detached signature of the presence status (announces our key to XEP-0027 clients).
pub fn sign_status(status: &str, secret: &SignedSecretKey) -> Result<String, String> {
    let sig = DetachedSignature::sign_binary_data(thread_rng(), &secret.primary_key, &Password::empty(), HashAlgorithm::Sha256, status.as_bytes()).map_err(err)?;
    Ok(strip_armor(&sig.to_armored_string(ArmorOptions::default()).map_err(err)?))
}

/// Issuer key id of a `jabber:x:signed` presence signature.
pub fn signature_issuer(payload: &str) -> Option<String> {
    let (sig, _) = DetachedSignature::from_string(&add_armor("SIGNATURE", payload)).ok()?;
    sig.signature.issuer_key_id().first().map(|id| hex_id(id))
}

/// Checks a presence signature against a contact's key (proves the key is theirs).
pub fn verify_status(status: &str, payload: &str, key: &SignedPublicKey) -> bool {
    let Ok((sig, _)) = DetachedSignature::from_string(&add_armor("SIGNATURE", payload)) else { return false };
    sig.verify(key, status.as_bytes()).is_ok() || key.public_subkeys.iter().any(|k| sig.verify(k, status.as_bytes()).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encrypt_decrypt_sign_roundtrip() {
        let alice = generate("alice <xmpp:alice@localhost>").unwrap();
        let bob = generate("bob <xmpp:bob@localhost>").unwrap();
        let (alice_pub, bob_pub) = (public_of(&alice), public_of(&bob));

        let payload = encrypt("тайна", &[&bob_pub, &alice_pub]).unwrap();
        assert!(!payload.contains("BEGIN"), "XEP-0027 strips armor framing");
        assert_eq!(decrypt(&payload, &bob).unwrap(), "тайна");
        assert_eq!(decrypt(&payload, &alice).unwrap(), "тайна", "sender can read own message");
        let eve = generate("eve").unwrap();
        assert!(decrypt(&payload, &eve).is_err());

        let sig = sign_status("Online", &alice).unwrap();
        assert!(verify_status("Online", &sig, &alice_pub));
        assert!(!verify_status("Away", &sig, &alice_pub));
        assert!(!verify_status("Online", &sig, &bob_pub));
        assert!(key_ids(&alice_pub).contains(&signature_issuer(&sig).unwrap()));

        // Keys survive armor export/import.
        assert_eq!(fingerprint(&parse_public(&armor_public(&alice_pub)).unwrap()), fingerprint(&alice_pub));
        assert_eq!(fingerprint(&public_of(&parse_secret(&armor_secret(&alice)).unwrap())), fingerprint(&alice_pub));
    }
}
