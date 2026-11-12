//! XEP-0388 Extensible SASL Profile, with XEP-0386 Bind 2 and XEP-0484 FAST tokens
//! (crabber patch). After `<success/>` there is no stream restart: the server sends fresh stream
//! features, which the stream negotiation then uses (SM resume or enable, legacy binding).
use alloc::collections::BTreeSet;
use std::io;
use std::sync::{Arc, Mutex};

use futures::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use sasl::common::{ChannelBinding, Credentials, Identity};
use sha2::Sha256;
use tokio::io::{AsyncBufRead, AsyncWrite};
use xmpp_parsers::{
    bind2::BindQuery,
    jid::FullJid,
    minidom::Element,
    ns,
    sasl2::{Authenticate, Response, UserAgent},
    stream_features::StreamFeatures,
};

use super::login::{cb_types, check_downgrade, pick_mechanism};
use crate::{
    error::{AuthError, Error},
    xmlstream::{ReadError, Sasl2Nonza, XmppStream, XmppStreamElement},
};

/// A XEP-0484 token: what it is for, the secret, and until when (as the server wrote it).
#[derive(Clone, PartialEq)]
pub struct FastToken {
    /// The HT-* mechanism the token works with.
    pub mechanism: String,
    /// The secret.
    pub token: String,
    /// When it runs out (XEP-0082 date-time).
    pub expiry: String,
}

/// Logs never show the secret.
impl core::fmt::Debug for FastToken {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FastToken").field("mechanism", &self.mechanism).field("token", &"…").field("expiry", &self.expiry).finish()
    }
}

/// How to log in beyond the password.
#[derive(Clone, Default)]
pub struct LoginOptions {
    /// XEP-0388 user agent: a UUID that stays the same for this installation, the software and
    /// the device. FAST tokens are tied to it.
    pub user_agent: Option<(uuid::Uuid, String, String)>,
    /// XEP-0386 tag for the resource the server picks.
    pub tag: Option<String>,
    /// XEP-0484: the token to log in with; replaced whenever the server hands out a new one,
    /// cleared when it is refused.
    pub fast: Option<Arc<Mutex<Option<FastToken>>>>,
}

/// SASL2 authentication; with `bind` and Bind 2 on offer, the resource is bound in the same go
/// and its full JID returned.
pub(crate) async fn auth<S: AsyncBufRead + AsyncWrite + Unpin>(
    mut stream: XmppStream<S>,
    features: &StreamFeatures,
    creds: Credentials,
    opts: &LoginOptions,
    bind: bool,
) -> Result<(StreamFeatures, XmppStream<S>, Option<FullJid>), Error> {
    let sasl2 = features.sasl2.as_ref().ok_or(AuthError::NoMechanism)?;
    let cb = cb_types(features);
    let offered: BTreeSet<String> = sasl2.mechanisms.iter().cloned().collect();
    let inline = sasl2.inline.as_ref();
    let fast_offer: Vec<String> = inline
        .into_iter()
        .flat_map(|i| &i.payloads)
        .filter(|p| p.is("fast", ns::FAST))
        .flat_map(|f| f.children().filter(|c| c.is("mechanism", ns::FAST)).map(|c| c.text()))
        .collect();
    let store = opts.fast.as_ref().filter(|_| !fast_offer.is_empty() && opts.user_agent.is_some());
    let username = match &creds.identity {
        Identity::Username(u) => u.clone(),
        Identity::None => String::new(),
    };
    let exporter = match &creds.channel_binding {
        ChannelBinding::TlsExporter(d) => Some(d.clone()),
        _ => None,
    };

    // A token we have for a mechanism the server takes, and its channel-binding data.
    let ht = store
        .and_then(|s| s.lock().unwrap().clone())
        .filter(|t| fast_offer.contains(&t.mechanism))
        .and_then(|t| ht_cb(&t.mechanism, exporter.as_deref()).map(|cb| (t, cb)));
    let mut payloads: Vec<Element> = vec![];
    let mut requested = None;
    let (mechanism, initial, mut scram) = match &ht {
        Some((t, cb)) => {
            payloads.push(Element::builder("fast", ns::FAST).build());
            let mut initial = username.clone().into_bytes();
            initial.push(0);
            initial.extend(ht_hmac(&t.token, b"Initiator", cb));
            (t.mechanism.clone(), initial, None)
        }
        None => {
            let mut m = pick_mechanism(&offered, cb.as_deref(), &creds).ok_or(AuthError::NoMechanism)?;
            if store.is_some() {
                requested = request_mechanism(&fast_offer, exporter.is_some());
            }
            if let Some(want) = &requested {
                payloads.push(Element::builder("request-token", ns::FAST).attr(rxml::xml_ncname!("mechanism").to_owned(), want.as_str()).build());
            }
            (m.name().to_string(), m.initial(), Some(m))
        }
    };
    if bind && inline.is_some_and(|i| i.bind2.is_some()) {
        payloads.push(BindQuery { tag: opts.tag.clone(), payloads: vec![] }.into());
    }
    let user_agent = match &opts.user_agent {
        Some((id, software, device)) => UserAgent { id: *id, software: Some(software.clone()), device: Some(device.clone()) },
        None => UserAgent { id: uuid::Uuid::from_bytes(rand::random()), software: None, device: None },
    };
    stream
        .send(&XmppStreamElement::Sasl2(Sasl2Nonza::Authenticate(Authenticate {
            mechanism: mechanism.clone(),
            initial_response: Some(initial),
            user_agent,
            payloads,
        })))
        .await?;

    let mut first = true;
    loop {
        match stream.next().await.map(|v| v.map(|v| v.into_read_error()).flatten()) {
            Some(Ok(XmppStreamElement::Sasl2(Sasl2Nonza::Challenge(c)))) => {
                let Some(m) = scram.as_mut() else {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "challenge to a FAST token").into());
                };
                if core::mem::take(&mut first) {
                    check_downgrade(&mechanism, &c.sasl_data, &sasl2.mechanisms, cb.as_deref())?;
                }
                let sasl_data = m.response(&c.sasl_data).map_err(AuthError::Sasl)?;
                stream.send(&XmppStreamElement::Sasl2(Sasl2Nonza::Response(Response { sasl_data }))).await?;
            }
            Some(Ok(XmppStreamElement::Sasl2(Sasl2Nonza::Success(s)))) => {
                let data = s.additional_data.clone().unwrap_or_default();
                let proven = match (&mut scram, &ht) {
                    (Some(m), _) => m.success(&data).is_ok(),
                    (None, Some((t, cb))) => ht_verify(&t.token, cb, &data),
                    (None, None) => false,
                };
                if !proven {
                    return Err(AuthError::ServerProof.into());
                }
                // A new token: the one we asked for, or a rotation of the one we used.
                if let (Some(store), Some(t)) = (store, s.payloads.iter().find(|p| p.is("token", ns::FAST))) {
                    let mechanism = requested.clone().or_else(|| ht.as_ref().map(|(t, _)| t.mechanism.clone()));
                    if let (Some(mechanism), Some(token)) = (mechanism, t.attr("token")) {
                        *store.lock().unwrap() = Some(FastToken {
                            mechanism,
                            token: token.to_string(),
                            expiry: t.attr("expiry").unwrap_or_default().to_string(),
                        });
                    }
                }
                let bound = s
                    .payloads
                    .iter()
                    .any(|p| p.is("bound", ns::BIND2))
                    .then(|| FullJid::try_from(s.authorization_identifier.clone()).ok())
                    .flatten();
                // No restart: the server goes on with the post-authentication features.
                loop {
                    match stream.next().await.map(|v| v.map(|v| v.into_read_error()).flatten()) {
                        Some(Ok(XmppStreamElement::Features(f))) => return Ok((f, stream, bound)),
                        Some(Ok(XmppStreamElement::StreamError(e))) => return Err(io::Error::other(e).into()),
                        Some(Ok(_)) | Some(Err(ReadError::SoftTimeout)) => (),
                        Some(Err(ReadError::HardError(e))) => return Err(e.into()),
                        Some(Err(ReadError::ParseError(e))) => return Err(io::Error::new(io::ErrorKind::InvalidData, e).into()),
                        Some(Err(ReadError::StreamFooterReceived)) | None => return Err(Error::Disconnected),
                    }
                }
            }
            Some(Ok(XmppStreamElement::Sasl2(Sasl2Nonza::Failure(f)))) => {
                // A refused token is no good any more: next time, the password.
                if let (Some(store), Some(_)) = (store, &ht) {
                    *store.lock().unwrap() = None;
                }
                let condition = f.payloads.first().map(|c| c.name().to_string()).unwrap_or_default();
                return Err(AuthError::Sasl2(f.text.unwrap_or(condition)).into());
            }
            Some(Ok(XmppStreamElement::StreamError(e))) => return Err(io::Error::other(e).into()),
            Some(Ok(el)) => {
                return Err(io::Error::new(io::ErrorKind::InvalidData, format!("unexpected stream element during SASL2: {el:?}")).into())
            }
            Some(Err(ReadError::SoftTimeout)) => (),
            Some(Err(ReadError::HardError(e))) => return Err(e.into()),
            Some(Err(ReadError::ParseError(e))) => return Err(io::Error::new(io::ErrorKind::InvalidData, e).into()),
            Some(Err(ReadError::StreamFooterReceived)) | None => return Err(Error::Disconnected),
        }
    }
}

/// The FAST mechanism to ask a token for: bound to the TLS channel when we can.
fn request_mechanism(offer: &[String], exporter: bool) -> Option<String> {
    ["HT-SHA-256-EXPR", "HT-SHA-256-NONE"]
        .into_iter()
        .filter(|m| exporter || !m.ends_with("EXPR"))
        .find(|m| offer.iter().any(|o| o == m))
        .map(String::from)
}

/// Channel-binding data of an HT mechanism (draft-schmaus-kitten-sasl-ht): none, or the TLS
/// exporter value (RFC 9266). Endpoint and unique bindings we cannot produce.
fn ht_cb(mechanism: &str, exporter: Option<&[u8]>) -> Option<Vec<u8>> {
    match mechanism {
        "HT-SHA-256-NONE" => Some(vec![]),
        "HT-SHA-256-EXPR" => exporter.map(<[u8]>::to_vec),
        _ => None,
    }
}

fn ht_hmac(token: &str, label: &[u8], cb: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(token.as_bytes()).expect("HMAC takes any key");
    mac.update(label);
    mac.update(cb);
    mac.finalize().into_bytes().to_vec()
}

/// The server's proof that it knows the token too.
fn ht_verify(token: &str, cb: &[u8], data: &[u8]) -> bool {
    let mut mac = Hmac::<Sha256>::new_from_slice(token.as_bytes()).expect("HMAC takes any key");
    mac.update(b"Responder");
    mac.update(cb);
    mac.verify_slice(data).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ht_mechanisms() {
        assert_eq!(request_mechanism(&["HT-SHA-256-NONE".into(), "HT-SHA-256-EXPR".into()], true).as_deref(), Some("HT-SHA-256-EXPR"));
        assert_eq!(request_mechanism(&["HT-SHA-256-NONE".into(), "HT-SHA-256-EXPR".into()], false).as_deref(), Some("HT-SHA-256-NONE"));
        assert_eq!(request_mechanism(&["HT-SHA-256-ENDP".into()], true), None);
        assert_eq!(ht_cb("HT-SHA-256-EXPR", None), None, "no exporter without TLS 1.3");
        // The responder proof is the HMAC over "Responder" and the binding data.
        let proof = ht_hmac("secret", b"Responder", b"cb");
        assert!(ht_verify("secret", b"cb", &proof));
        assert!(!ht_verify("secret", b"other", &proof));
        assert_ne!(ht_hmac("secret", b"Initiator", b""), ht_hmac("secret", b"Responder", b""));
    }
}
