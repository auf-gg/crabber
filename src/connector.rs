//! Connector with optional manual host/port and an opt-in for self-signed certificates.
//! Direct TLS (XEP-0368) where the domain publishes `_xmpps-client._tcp`, STARTTLS otherwise.
//! Mirrors tokio_xmpp's StartTls/DirectTls connectors, which hardcode the TLS config.
use std::borrow::Cow;
use std::io;
use std::sync::Arc;

use futures::{SinkExt, StreamExt};
use sasl::common::ChannelBinding;
use tokio::io::BufStream;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::{
    self, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use tokio_xmpp::connect::{DnsConfig, ServerConnector};
use tokio_xmpp::jid::Jid;
use tokio_xmpp::parsers::starttls::{Nonza, Request};
use tokio_xmpp::xmlstream::{FallibleStreamElement, PendingFeaturesRecv, ReadError, StreamHeader, Timeouts, XmppStreamElement, initiate_stream};
use tokio_xmpp::{Error, error::ProtocolError};

#[derive(Debug, Clone)]
pub struct Connector {
    /// Manual server host; `None` means SRV lookup on the JID domain.
    pub host: Option<String>,
    pub port: u16,
    /// Accept any server certificate (self-signed ejabberd setups).
    pub insecure: bool,
}

impl Connector {
    pub fn tls_config(insecure: bool) -> ClientConfig {
        if insecure {
            return ClientConfig::builder()
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AcceptAny))
                .with_no_client_auth();
        }
        let mut roots = RootCertStore::empty();
        roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
        ClientConfig::builder().with_root_certificates(roots).with_no_client_auth()
    }
}

impl ServerConnector for Connector {
    type Stream = BufStream<TlsStream<TcpStream>>;

    async fn connect(
        &self,
        jid: &Jid,
        ns: &'static str,
        timeouts: Timeouts,
    ) -> Result<(PendingFeaturesRecv<Self::Stream>, ChannelBinding), Error> {
        let domain = jid.domain().as_str();
        let header = || StreamHeader { to: Some(Cow::Owned(domain.to_owned())), from: None, id: None };

        // XEP-0368: Direct TLS first, on the domain's xmpps SRV targets or a manual host on 5223.
        let direct = match &self.host {
            Some(host) if self.port == DIRECT_TLS_PORT => vec![(host.clone(), self.port)],
            Some(_) => vec![],
            None => xmpps_targets(domain).await,
        };
        for (host, port) in direct {
            let attempt = async {
                let tcp = DnsConfig::no_srv(&host, port).resolve().await?;
                Ok::<_, Error>(handshake(tcp, domain, self.insecure, true).await?)
            };
            if let Ok(Ok((tls, binding))) = tokio::time::timeout(CONNECT_TIMEOUT, attempt).await {
                return Ok((initiate_stream(BufStream::new(tls), ns, header(), timeouts).await?, binding));
            }
        }

        let dns = match &self.host {
            Some(host) => DnsConfig::no_srv(host, self.port),
            None => DnsConfig::srv_default_client(domain),
        };
        let tcp = BufStream::new(dns.resolve().await?);
        let (features, mut stream) = initiate_stream(tcp, ns, header(), timeouts).await?.recv_features::<FallibleStreamElement>().await?;
        if !features.can_starttls() {
            return Err(Error::Protocol(ProtocolError::NoTls));
        }
        stream.send(&XmppStreamElement::Starttls(Nonza::Request(Request))).await?;
        loop {
            match stream.next().await.map(|v| v.and_then(|v| v.into_read_error())) {
                Some(Ok(XmppStreamElement::Starttls(Nonza::Proceed(_)))) => break,
                Some(Ok(_)) | Some(Err(ReadError::SoftTimeout)) => (),
                Some(Err(ReadError::HardError(e))) => return Err(e.into()),
                Some(Err(ReadError::ParseError(e))) => return Err(io::Error::new(io::ErrorKind::InvalidData, e).into()),
                None | Some(Err(ReadError::StreamFooterReceived)) => return Err(Error::Disconnected),
            }
        }

        let (tls, binding) = handshake(stream.into_inner().into_inner(), domain, self.insecure, false).await?;
        Ok((initiate_stream(BufStream::new(tls), ns, header(), timeouts).await?, binding))
    }
}

/// The port servers listen on for Direct TLS when not told otherwise.
const DIRECT_TLS_PORT: u16 = 5223;
/// A Direct TLS target that does not answer must not hold up the STARTTLS fallback for long.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// `_xmpps-client._tcp` SRV targets of `domain`, best first; none when it publishes none.
async fn xmpps_targets(domain: &str) -> Vec<(String, u16)> {
    let Ok(resolver) = hickory_resolver::TokioResolver::builder_tokio().and_then(|b| b.build()) else { return vec![] };
    let Ok(lookup) = resolver.srv_lookup(format!("_xmpps-client._tcp.{domain}.")).await else { return vec![] };
    let mut records: Vec<_> = lookup
        .answers()
        .iter()
        .filter_map(|r| match &r.data {
            hickory_resolver::proto::rr::RData::SRV(srv) => Some((srv.priority, srv.target.to_ascii(), srv.port)),
            _ => None,
        })
        // A "." target: the service is decidedly not offered (RFC 2782).
        .filter(|(_, target, _)| target != ".")
        .collect();
    records.sort_by_key(|r| r.0);
    records.into_iter().map(|(_, target, port)| (target.trim_end_matches('.').to_string(), port)).collect()
}

/// TLS on `stream` for `domain`, with the RFC 9266 tls-exporter channel binding when TLS 1.3.
/// `alpn`: Direct TLS names the protocol, so servers sharing port 443 can tell XMPP apart.
async fn handshake(stream: TcpStream, domain: &str, insecure: bool, alpn: bool) -> io::Result<(TlsStream<TcpStream>, ChannelBinding)> {
    let server_name = ServerName::try_from(domain.to_owned()).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let mut config = Connector::tls_config(insecure);
    if alpn {
        config.alpn_protocols = vec![b"xmpp-client".to_vec()];
    }
    let tls = TlsConnector::from(Arc::new(config)).connect(server_name, stream).await?;
    let (_, conn) = tls.get_ref();
    let binding = match conn.protocol_version() {
        Some(rustls::ProtocolVersion::TLSv1_3) => conn
            .export_keying_material(vec![0u8; 32], b"EXPORTER-Channel-Binding", None)
            .map(ChannelBinding::TlsExporter)
            .unwrap_or(ChannelBinding::None),
        _ => ChannelBinding::None,
    };
    Ok((tls, binding))
}

#[cfg(test)]
mod tests {
    /// Needs the network: public servers that publish Direct TLS answer with stream features
    /// (yax.im on port 443, where the server tells XMPP from HTTPS by ALPN).
    #[test]
    #[ignore]
    fn direct_tls_to_public_servers() {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        for domain in ["yax.im", "disroot.org"] {
            rt.block_on(async {
                let targets = super::xmpps_targets(domain).await;
                assert!(!targets.is_empty(), "{domain}: no _xmpps-client._tcp records");
                let (host, port) = &targets[0];
                let features = tokio::time::timeout(std::time::Duration::from_secs(20), async {
                    let tcp = tokio_xmpp::connect::DnsConfig::no_srv(host, *port).resolve().await.unwrap();
                    let (tls, _) = super::handshake(tcp, domain, false, true).await.unwrap();
                    let header = tokio_xmpp::xmlstream::StreamHeader { to: Some(domain.into()), from: None, id: None };
                    let timeouts = tokio_xmpp::xmlstream::Timeouts { read_timeout: std::time::Duration::from_secs(20), response_timeout: std::time::Duration::from_secs(20) };
                    let pending = tokio_xmpp::xmlstream::initiate_stream(tokio::io::BufStream::new(tls), "jabber:client", header, timeouts).await.unwrap();
                    pending.recv_features::<tokio_xmpp::xmlstream::FallibleStreamElement>().await.unwrap().0
                })
                .await
                .unwrap_or_else(|_| panic!("{domain}: timed out"));
                assert!(!features.can_starttls(), "{domain}: already encrypted, no STARTTLS offered");
            });
        }
    }
}

#[derive(Debug)]
struct AcceptAny;

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(&self, _: &[u8], _: &CertificateDer<'_>, _: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider().signature_verification_algorithms.supported_schemes()
    }
}
