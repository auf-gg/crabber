use alloc::borrow::Cow;
use core::str::FromStr;
use futures::{SinkExt, StreamExt};
use sasl::client::mechanisms::{Plain, Scram};
use sasl::client::Mechanism;
use sasl::common::scram::{Sha1, Sha256};
use sasl::common::{ChannelBinding, Credentials};
use std::collections::BTreeSet;
use std::io;
use tokio::io::{AsyncBufRead, AsyncWrite};
use xmpp_parsers::{
    jid::{FullJid, Jid},
    ns,
    sasl::{Auth, Mechanism as XMPPMechanism, Nonza, Response},
    stream_features::StreamFeatures,
};

use crate::{
    connect::ServerConnector,
    error::{AuthError, Error, ProtocolError},
    xmlstream::{
        xmpp::XmppStreamElement, InitiatingStream, ReadError, StreamHeader, Timeouts, XmppStream,
    },
};

pub use super::sasl2::{FastToken, LoginOptions};

/// Run the authentication handshake on a given stream.
///
/// Uses the given `sasl_mechanisms` and `creds` to perform the full
/// authentication handshake. As authentication ends with a stream reset,
/// this returns the `stream` as [`InitiatingStream`] on success.
pub async fn auth<S: AsyncBufRead + AsyncWrite + Unpin>(
    stream: XmppStream<S>,
    sasl_mechanisms: BTreeSet<String>,
    creds: Credentials,
) -> Result<InitiatingStream<S>, Error> {
    auth_with(stream, &sasl_mechanisms, None, creds).await
}

/// SASL (RFC 6120) with the mechanism `pick_mechanism` chooses (crabber patch: the choice, the
/// XEP-0474 check and the server signature check).
async fn auth_with<S: AsyncBufRead + AsyncWrite + Unpin>(
    mut stream: XmppStream<S>,
    sasl_mechanisms: &BTreeSet<String>,
    cb_types: Option<&[String]>,
    creds: Credentials,
) -> Result<InitiatingStream<S>, Error> {
    let mut mechanism = pick_mechanism(sasl_mechanisms, cb_types, &creds).ok_or(AuthError::NoMechanism)?;
    let offered: Vec<String> = sasl_mechanisms.iter().cloned().collect();
    let initial = mechanism.initial();
    let mechanism_name = XMPPMechanism::from_str(mechanism.name()).map_err(ProtocolError::Parsers)?;

    stream
        .send(&XmppStreamElement::Sasl(Nonza::Auth(Auth {
            mechanism: mechanism_name,
            data: initial,
        })))
        .await?;

    let mut first = true;
    loop {
        match stream
            .next()
            .await
            .map(|v| v.map(|v| v.into_read_error()).flatten())
        {
            Some(Ok(XmppStreamElement::Sasl(sasl))) => match sasl {
                Nonza::Challenge(challenge) => {
                    if core::mem::take(&mut first) {
                        check_downgrade(mechanism.name(), &challenge.data, &offered, cb_types)?;
                    }
                    let response = mechanism
                        .response(&challenge.data)
                        .map_err(AuthError::Sasl)?;

                    // Send response and loop
                    stream
                        .send(&XmppStreamElement::Sasl(Nonza::Response(Response {
                            data: response,
                        })))
                        .await?;
                }
                Nonza::Success(success) => {
                    // SCRAM: the server signature proves the server knows the password too.
                    mechanism
                        .success(&success.data)
                        .map_err(|_| AuthError::ServerProof)?;
                    return Ok(stream.initiate_reset());
                }
                Nonza::Failure(failure) => {
                    return Err(Error::Auth(AuthError::Fail(failure.defined_condition)));
                }
                _ => {
                    // Ignore?!
                }
            },
            Some(Ok(el)) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "unexpected stream element during SASL negotiation: {:?}",
                        el
                    ),
                )
                .into())
            }
            Some(Err(ReadError::HardError(e))) => return Err(e.into()),
            Some(Err(ReadError::ParseError(e))) => {
                return Err(io::Error::new(io::ErrorKind::InvalidData, e).into())
            }
            Some(Err(ReadError::SoftTimeout)) => {
                // We cannot do anything about soft timeouts here...
            }
            Some(Err(ReadError::StreamFooterReceived)) | None => {
                return Err(Error::Disconnected)
            }
        }
    }
}

/// The password mechanism to use (crabber patch). SCRAM bound to the TLS channel (-PLUS) when
/// the server offers it for a binding we have; else SCRAM saying "y" (we could have bound it,
/// RFC 5802 §6) or "n" (we cannot); PLAIN only on servers without SCRAM. Upstream fell through to
/// PLAIN whenever we had a binding but the server no -PLUS mechanism.
pub(crate) fn pick_mechanism(
    offered: &BTreeSet<String>,
    cb_types: Option<&[String]>,
    creds: &Credentials,
) -> Option<Box<dyn Mechanism + Send + Sync>> {
    let exporter = matches!(creds.channel_binding, ChannelBinding::TlsExporter(_))
        && cb_types.is_none_or(|t| t.iter().any(|t| t == "tls-exporter"));
    let any_plus = offered.iter().any(|m| m.ends_with("-PLUS"));
    let unbound = if exporter && !any_plus {
        ChannelBinding::Unsupported
    } else {
        ChannelBinding::None
    };
    let with = |cb: ChannelBinding| creds.clone().with_channel_binding(cb);
    if exporter && offered.contains("SCRAM-SHA-256-PLUS") {
        return Some(Box::new(Scram::<Sha256>::from_credentials(creds.clone()).ok()?));
    }
    if exporter && offered.contains("SCRAM-SHA-1-PLUS") {
        return Some(Box::new(Scram::<Sha1>::from_credentials(creds.clone()).ok()?));
    }
    if offered.contains("SCRAM-SHA-256") {
        return Some(Box::new(Scram::<Sha256>::from_credentials(with(unbound)).ok()?));
    }
    if offered.contains("SCRAM-SHA-1") {
        return Some(Box::new(Scram::<Sha1>::from_credentials(with(unbound)).ok()?));
    }
    if offered.contains("PLAIN") {
        return Some(Box::new(Plain::from_credentials(creds.clone()).ok()?));
    }
    None
}

/// XEP-0474 (crabber patch): a SCRAM server-first-message may carry `h=`, the hash of every
/// mechanism (and channel-binding type) the server offered. A mismatch means someone between us
/// stripped the strong ones off, so we stop before sending our proof.
pub(crate) fn check_downgrade(
    mechanism: &str,
    server_first: &[u8],
    offered: &[String],
    cb_types: Option<&[String]>,
) -> Result<(), Error> {
    use base64::Engine as _;
    use sha2::Digest as _;
    let Ok(text) = core::str::from_utf8(server_first) else {
        return Ok(());
    };
    let Some(h) = text.split(',').find_map(|a| a.strip_prefix("h=")) else {
        return Ok(());
    };
    let mut mechs: Vec<&str> = offered.iter().map(String::as_str).collect();
    mechs.sort_unstable();
    let mut s = mechs.join("\u{1e}");
    if let Some(types) = cb_types {
        let mut types: Vec<&str> = types.iter().map(String::as_str).collect();
        types.sort_unstable();
        s.push('\u{1f}');
        s.push_str(&types.join("\u{1e}"));
    }
    let b64 = |d: &[u8]| base64::engine::general_purpose::STANDARD.encode(d);
    let expected = if mechanism.starts_with("SCRAM-SHA-256") {
        b64(&sha2::Sha256::digest(s.as_bytes()))
    } else if mechanism.starts_with("SCRAM-SHA-1") {
        b64(&sha1::Sha1::digest(s.as_bytes()))
    } else {
        return Ok(());
    };
    if expected == h {
        Ok(())
    } else {
        Err(AuthError::Downgrade.into())
    }
}

/// The XEP-0440 channel-binding types the server lists, as written on the wire.
pub(crate) fn cb_types(features: &StreamFeatures) -> Option<Vec<String>> {
    use xso::AsXmlText;
    let cb = features.sasl_cb.as_ref()?;
    Some(
        cb.types
            .iter()
            .filter_map(|t| t.as_xml_text().ok().map(|t| t.into_owned()))
            .collect(),
    )
}

/// Authenticate (crabber patch): SASL2 (XEP-0388) where the server has it, with a FAST token
/// (XEP-0484) when there is one and, with `bind`, the resource bound right away (XEP-0386,
/// the bound JID comes back). Classic SASL otherwise.
pub async fn client_login<C: ServerConnector>(
    server: C,
    jid: Jid,
    password: String,
    timeouts: Timeouts,
    opts: &LoginOptions,
    bind: bool,
) -> Result<(StreamFeatures, XmppStream<C::Stream>, Option<FullJid>), Error> {
    let username = jid.node().unwrap().as_str();

    let (xmpp_stream, channel_binding) = server.connect(&jid, ns::JABBER_CLIENT, timeouts).await?;
    let (features, xmpp_stream) = xmpp_stream.recv_features().await?;

    let creds = Credentials::default()
        .with_username(username)
        .with_password(password)
        .with_channel_binding(channel_binding);
    if features.sasl2.is_some() {
        return super::sasl2::auth(xmpp_stream, &features, creds, opts, bind).await;
    }
    // Authenticated (unspecified) stream
    let cb_types = cb_types(&features);
    let stream = auth_with(xmpp_stream, &features.sasl_mechanisms, cb_types.as_deref(), creds).await?;
    let stream = stream
        .send_header(StreamHeader {
            to: Some(Cow::Borrowed(jid.domain().as_str())),
            from: None,
            id: None,
        })
        .await?;
    let (features, stream) = stream.recv_features().await?;
    Ok((features, stream, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offered(m: &[&str]) -> BTreeSet<String> {
        m.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn mechanism_choice() {
        let bound = Credentials::default()
            .with_username("u")
            .with_password("p")
            .with_channel_binding(ChannelBinding::TlsExporter(vec![1; 32]));
        let name = |o: &[&str], cb: Option<&[String]>, c: &Credentials| pick_mechanism(&offered(o), cb, c).map(|m| m.name().to_string());
        assert_eq!(name(&["PLAIN", "SCRAM-SHA-1", "SCRAM-SHA-256", "SCRAM-SHA-256-PLUS"], None, &bound).as_deref(), Some("SCRAM-SHA-256-PLUS"));
        // No -PLUS offered: SCRAM all the same (upstream went to PLAIN here).
        assert_eq!(name(&["PLAIN", "SCRAM-SHA-1"], None, &bound).as_deref(), Some("SCRAM-SHA-1"));
        // The server binds only tls-server-end-point, which we lack.
        let endp = ["tls-server-end-point".to_string()];
        assert_eq!(name(&["SCRAM-SHA-256", "SCRAM-SHA-256-PLUS"], Some(&endp), &bound).as_deref(), Some("SCRAM-SHA-256"));
        assert_eq!(name(&["PLAIN"], None, &bound).as_deref(), Some("PLAIN"));
        assert!(name(&["X-OAUTH2"], None, &bound).is_none());
    }

    #[test]
    fn scram_gs2_flag() {
        // "y": we can bind but the server offers no -PLUS; "n": we cannot bind what it offers.
        let bound = Credentials::default().with_username("u").with_password("p").with_channel_binding(ChannelBinding::TlsExporter(vec![1; 32]));
        let first = |o: &[&str], cb: Option<&[String]>| String::from_utf8(pick_mechanism(&offered(o), cb, &bound).unwrap().initial()).unwrap();
        assert!(first(&["SCRAM-SHA-256"], None).starts_with("y,,"));
        assert!(first(&["SCRAM-SHA-256", "SCRAM-SHA-256-PLUS"], Some(&["tls-server-end-point".to_string()])).starts_with("n,,"));
        assert!(first(&["SCRAM-SHA-256-PLUS"], None).starts_with("p=tls-exporter,,"));
    }

    /// XEP-0474 §6: the hash covers the sorted mechanisms, then the sorted channel-binding types.
    #[test]
    fn downgrade_protection() {
        use base64::Engine as _;
        use sha2::Digest as _;
        let mechs = ["SCRAM-SHA-256", "SCRAM-SHA-1", "SCRAM-SHA-256-PLUS"].map(String::from);
        let types = ["tls-server-end-point", "tls-exporter"].map(String::from);
        let s = "SCRAM-SHA-1\u{1e}SCRAM-SHA-256\u{1e}SCRAM-SHA-256-PLUS\u{1f}tls-exporter\u{1e}tls-server-end-point";
        let h = base64::engine::general_purpose::STANDARD.encode(sha2::Sha256::digest(s.as_bytes()));
        let first = format!("r=abc,s=c2FsdA==,i=4096,h={h}");
        assert!(check_downgrade("SCRAM-SHA-256", first.as_bytes(), &mechs, Some(&types)).is_ok());
        // Someone stripped SCRAM-SHA-256-PLUS from what we saw.
        let stripped = ["SCRAM-SHA-256", "SCRAM-SHA-1"].map(String::from);
        assert!(check_downgrade("SCRAM-SHA-256", first.as_bytes(), &stripped, Some(&types)).is_err());
        // Servers without XEP-0474 send no h=.
        assert!(check_downgrade("SCRAM-SHA-256", b"r=abc,s=c2FsdA==,i=4096", &stripped, None).is_ok());
    }
}
