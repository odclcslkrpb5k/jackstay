//! TLS and QUIC configuration (docs/link.md, "Transport"): TLS 1.3 only, Ed25519 raw public keys
//! (RFC 7250) on both ends, no resumption, and endpoints whose reset and connection-ID keys are
//! derived from the identity, so a restarted daemon can reset its old sessions.

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls13_signature_with_raw_key};
use rustls::pki_types::{CertificateDer, ServerName, SubjectPublicKeyInfoDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};

use crate::Timings;
use crate::identity::{Derived, Identity, PublicKey};

/// The link's application protocol; a peer of another version fails the handshake.
pub const ALPN: &[u8] = b"jackstay/1";
/// The pairing endpoint's application protocol.
pub const PAIR_ALPN: &[u8] = b"jackstay-pair/1";
/// The server name a dialer sends. It means nothing and is not checked: keys are the identity.
pub(crate) const SERVER_NAME: &str = "jackstay";
/// The most handshakes an endpoint holds at once (quinn's default is 65,536).
const MAX_INCOMING: usize = 16;

/// Whose keys a verifier accepts.
#[derive(Debug, Clone)]
pub(crate) enum Accept {
    /// Exactly these: the paired peers.
    Pinned(Arc<[PublicKey]>),
    /// Any key but this one, our own: pairing.
    AnyBut(PublicKey),
}

#[derive(Debug)]
struct Verifier {
    accept: Accept,
    provider: Arc<CryptoProvider>,
}

impl Verifier {
    fn check(&self, end_entity: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        let refused = || rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer);
        let key = PublicKey::from_spki(end_entity.as_ref()).ok_or_else(refused)?;
        let ok = match &self.accept {
            Accept::Pinned(keys) => keys.contains(&key),
            Accept::AnyBut(own) => key != *own,
        };
        if ok { Ok(()) } else { Err(refused()) }
    }

    fn verify13(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature_with_raw_key(
            message,
            &SubjectPublicKeyInfoDer::from(cert.as_ref()),
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }
}

fn tls12_refused() -> Result<HandshakeSignatureValid, rustls::Error> {
    Err(rustls::Error::PeerIncompatible(rustls::PeerIncompatible::Tls12NotOffered))
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity).map(|()| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        tls12_refused()
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verify13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

impl ClientCertVerifier for Verifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity).map(|()| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        tls12_refused()
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verify13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }

    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn verifier(accept: Accept) -> Arc<Verifier> {
    Arc::new(Verifier { accept, provider: provider() })
}

fn transport(timings: &Timings) -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.keep_alive_interval(Some(timings.keep_alive));
    // An idle timeout too long for QUIC's varint is no timeout at all; ours are seconds.
    t.max_idle_timeout(quinn::IdleTimeout::try_from(timings.idle_timeout).ok());
    Arc::new(t)
}

fn tls_error(e: &rustls::Error) -> io::Error {
    io::Error::other(format!("TLS configuration: {e}"))
}

/// The configuration for answering handshakes: our key, the peers `accept` allows, `alpn`.
pub(crate) fn server_config(
    identity: &Identity,
    accept: Accept,
    alpn: &[u8],
    timings: &Timings,
) -> io::Result<quinn::ServerConfig> {
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| tls_error(&e))?
        .with_client_cert_verifier(verifier(accept))
        .with_cert_resolver(Arc::new(rustls::server::AlwaysResolvesServerRawPublicKeys::new(
            identity.certified(),
        )));
    tls.alpn_protocols = vec![alpn.to_vec()];
    // No resumption: a resumed session takes the client's key from the ticket without asking the
    // verifier (docs/link.md, "Transport").
    tls.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    tls.send_tls13_tickets = 0;
    let quic = QuicServerConfig::try_from(tls).map_err(|e| io::Error::other(e.to_string()))?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(quic));
    config.transport_config(transport(timings));
    config.max_incoming(MAX_INCOMING);
    Ok(config)
}

/// The configuration for dialing: our key, the peers `accept` allows, `alpn`.
pub(crate) fn client_config(
    identity: &Identity,
    accept: Accept,
    alpn: &[u8],
    timings: &Timings,
) -> io::Result<quinn::ClientConfig> {
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(|e| tls_error(&e))?
        .dangerous()
        .with_custom_certificate_verifier(verifier(accept))
        .with_client_cert_resolver(Arc::new(
            rustls::client::AlwaysResolvesClientRawPublicKeys::new(identity.certified()),
        ));
    tls.alpn_protocols = vec![alpn.to_vec()];
    tls.resumption = rustls::client::Resumption::disabled();
    let quic = QuicClientConfig::try_from(tls).map_err(|e| io::Error::other(e.to_string()))?;
    let mut config = quinn::ClientConfig::new(Arc::new(quic));
    config.transport_config(transport(timings));
    Ok(config)
}

/// A UDP socket on `port` for both IPv6 and IPv4 (`IPV6_V6ONLY` off, whatever the sysctl says), or
/// IPv4 only where IPv6 is unavailable.
fn socket(port: u16) -> io::Result<std::net::UdpSocket> {
    use socket2::{Domain, Protocol, Socket, Type};
    let dual = || -> io::Result<Socket> {
        let s = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
        s.set_only_v6(false)?;
        s.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)).into())?;
        Ok(s)
    };
    let socket = match dual() {
        Ok(s) => s,
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => return Err(e),
        Err(_) => {
            let s = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))?;
            s.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)).into())?;
            s
        }
    };
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}

/// A QUIC endpoint on `port`, answering with `server` if given.
pub(crate) fn endpoint(
    identity: &Identity,
    port: u16,
    server: Option<quinn::ServerConfig>,
) -> io::Result<quinn::Endpoint> {
    let reset =
        ring::hmac::Key::new(ring::hmac::HMAC_SHA256, &identity.derive(Derived::StatelessReset));
    let mut config = quinn::EndpointConfig::new(Arc::new(reset));
    let derived = identity.derive(Derived::ConnectionId);
    let cid_key = u64::from_be_bytes(derived.first_chunk::<8>().copied().unwrap_or_default());
    config.cid_generator(move || {
        Box::new(quinn_proto::HashedConnectionIdGenerator::from_key(cid_key))
    });
    quinn::Endpoint::new(config, server, socket(port)?, Arc::new(quinn::TokioRuntime))
}
