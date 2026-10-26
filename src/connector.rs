//! STARTTLS connector with optional manual host/port and an opt-in for self-signed certificates.
//! Mirrors tokio_xmpp::connect::StartTlsServerConnector, which hardcodes the TLS config.
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
        let dns = match &self.host {
            Some(host) => DnsConfig::no_srv(host, self.port),
            None => DnsConfig::srv_default_client(domain),
        };
        let header = || StreamHeader { to: Some(Cow::Owned(domain.to_owned())), from: None, id: None };

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

        let server_name = ServerName::try_from(domain.to_owned())
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        let tls = TlsConnector::from(Arc::new(Self::tls_config(self.insecure)))
            .connect(server_name, stream.into_inner().into_inner())
            .await?;
        let (_, conn) = tls.get_ref();
        let binding = match conn.protocol_version() {
            Some(rustls::ProtocolVersion::TLSv1_3) => conn
                .export_keying_material(vec![0u8; 32], b"EXPORTER-Channel-Binding", None)
                .map(ChannelBinding::TlsExporter)
                .unwrap_or(ChannelBinding::None),
            _ => ChannelBinding::None,
        };
        Ok((initiate_stream(BufStream::new(tls), ns, header(), timeouts).await?, binding))
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
