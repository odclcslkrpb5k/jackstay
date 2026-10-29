//! M3 spike: can quinn 0.11 + rustls 0.23 (ring) authenticate both ends by raw Ed25519 public keys
//! (RFC 7250), refuse an unknown key in the handshake, and give both ends the same exporter value
//! for a pairing code? And what does a small message's round trip cost on loopback?
//!
//! Run: `./dev bash -c 'cd spikes/link-probe && cargo run --release'`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use ring::rand::SystemRandom;
use ring::signature::Ed25519KeyPair;
use rustls::DigitallySignedStruct;
use rustls::DistinguishedName;
use rustls::SignatureScheme;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{WebPkiSupportedAlgorithms, verify_tls13_signature_with_raw_key};
use rustls::pki_types::{
    CertificateDer, PrivatePkcs8KeyDer, ServerName, SubjectPublicKeyInfoDer, UnixTime,
};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::sign::CertifiedKey;

/// One machine's identity: its key pair, as rustls signs with it, and its public key (SPKI DER).
struct Identity {
    key: Arc<CertifiedKey>,
    spki: Vec<u8>,
}

fn identity() -> Identity {
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
    let der = PrivatePkcs8KeyDer::from(pkcs8.as_ref().to_vec());
    let signer = rustls::crypto::ring::sign::any_eddsa_type(&der).unwrap();
    let spki = signer.public_key().unwrap().as_ref().to_vec();
    let key = Arc::new(CertifiedKey::new(vec![CertificateDer::from(spki.clone())], signer));
    Identity { key, spki }
}

fn algorithms() -> WebPkiSupportedAlgorithms {
    rustls::crypto::ring::default_provider().signature_verification_algorithms
}

/// Accepts exactly the pinned keys, or any key while pairing.
#[derive(Debug)]
struct Pinned {
    keys: Vec<Vec<u8>>,
    pairing: bool,
}

impl Pinned {
    fn check(&self, end_entity: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        if self.pairing || self.keys.iter().any(|k| k.as_slice() == end_entity.as_ref()) {
            Ok(())
        } else {
            Err(rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer))
        }
    }
    fn verify13(
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature_with_raw_key(
            message,
            &SubjectPublicKeyInfoDer::from(cert.as_ref()),
            dss,
            &algorithms(),
        )
    }
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity).map(|()| ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _m: &[u8],
        _c: &CertificateDer<'_>,
        _d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2".into()))
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Pinned::verify13(m, c, d)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

impl ClientCertVerifier for Pinned {
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
        _m: &[u8],
        _c: &CertificateDer<'_>,
        _d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(rustls::Error::General("TLS 1.2".into()))
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Pinned::verify13(m, c, d)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

const ALPN: &[u8] = b"jackstay/0";

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn server_config(me: &Identity, peers: Pinned) -> quinn::ServerConfig {
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .with_client_cert_verifier(Arc::new(peers))
        .with_cert_resolver(Arc::new(rustls::server::AlwaysResolvesServerRawPublicKeys::new(
            Arc::clone(&me.key),
        )));
    tls.alpn_protocols = vec![ALPN.to_vec()];
    quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls).unwrap()))
}

fn client_config(me: &Identity, peers: Pinned) -> quinn::ClientConfig {
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(peers))
        .with_client_cert_resolver(Arc::new(
            rustls::client::AlwaysResolvesClientRawPublicKeys::new(Arc::clone(&me.key)),
        ));
    tls.alpn_protocols = vec![ALPN.to_vec()];
    quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls).unwrap()))
}

fn peer_key(c: &quinn::Connection) -> Vec<u8> {
    let any = c.peer_identity().unwrap();
    let certs = any.downcast::<Vec<CertificateDer<'static>>>().unwrap();
    certs[0].as_ref().to_vec()
}

fn code(c: &quinn::Connection) -> [u8; 8] {
    let mut out = [0u8; 8];
    c.export_keying_material(&mut out, b"jackstay pairing", b"").unwrap();
    out
}

/// Server `s` with `server_peers`, client `c` with `client_peers`; returns the outcome on each end.
async fn handshake(
    s: &Identity,
    server_peers: Pinned,
    c: &Identity,
    client_peers: Pinned,
) -> (Result<quinn::Connection, String>, Result<quinn::Connection, String>) {
    let server =
        quinn::Endpoint::server(server_config(s, server_peers), "127.0.0.1:0".parse().unwrap())
            .unwrap();
    let addr: SocketAddr = server.local_addr().unwrap();
    let accept = tokio::spawn(async move {
        let incoming = server.accept().await.unwrap();
        let r = incoming.await.map_err(|e| e.to_string());
        // Keep the endpoint alive while the caller looks at the connection.
        (r, server)
    });
    let mut client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    client.set_default_client_config(client_config(c, client_peers));
    let cr = match client.connect(addr, "peer") {
        Ok(connecting) => connecting.await.map_err(|e| e.to_string()),
        Err(e) => Err(e.to_string()),
    };
    // A client that accepted the server may still be refused by it; give the refusal time to land.
    let (sr, server) =
        tokio::time::timeout(Duration::from_secs(5), accept).await.unwrap().unwrap();
    let cr = match cr {
        Ok(conn) => {
            tokio::time::sleep(Duration::from_millis(200)).await;
            match conn.close_reason() {
                Some(why) => Err(format!("closed after handshake: {why}")),
                None => Ok(conn),
            }
        }
        e => e,
    };
    std::mem::forget(server);
    std::mem::forget(client);
    (sr, cr)
}

async fn latency(conn_c: quinn::Connection, conn_s: quinn::Connection) {
    let echo = tokio::spawn(async move {
        let (mut send, mut recv) = conn_s.accept_bi().await.unwrap();
        let mut buf = [0u8; 16];
        while recv.read_exact(&mut buf).await.is_ok() {
            send.write_all(&buf).await.unwrap();
        }
    });
    let (mut send, mut recv) = conn_c.open_bi().await.unwrap();
    let mut buf = [0u8; 16];
    let mut times = Vec::new();
    for i in 0..20_000u32 {
        let t = Instant::now();
        send.write_all(&[i as u8; 16]).await.unwrap();
        recv.read_exact(&mut buf).await.unwrap();
        if i >= 1000 {
            times.push(t.elapsed());
        }
    }
    times.sort();
    let at = |q: f64| times[((times.len() as f64 - 1.0) * q) as usize];
    println!(
        "round trip, 16 B, loopback, n={}: p50 {:?}  p99 {:?}  p99.9 {:?}  max {:?}",
        times.len(),
        at(0.5),
        at(0.99),
        at(0.999),
        times[times.len() - 1]
    );
    send.finish().unwrap();
    drop(echo);
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let a = identity();
    let b = identity();
    let stranger = identity();
    let pin = |k: &Identity| Pinned { keys: vec![k.spki.clone()], pairing: false };
    let pairing = || Pinned { keys: vec![], pairing: true };
    println!("SPKI length {} bytes", a.spki.len());

    // 1. Both pinned: connects, each sees the other's key, exporters agree.
    let (s, c) = handshake(&a, pin(&b), &b, pin(&a)).await;
    let (s, c) = (s.unwrap(), c.unwrap());
    println!("1 pinned both ways: ok");
    println!("  server sees client key: {}", peer_key(&s) == b.spki);
    println!("  client sees server key: {}", peer_key(&c) == a.spki);
    println!("  exporter equal: {} ({:02x?})", code(&s) == code(&c), code(&s));

    // 2. A second session between the same keys gives a different exporter.
    let (s2, c2) = handshake(&a, pin(&b), &b, pin(&a)).await;
    let (s2, c2) = (s2.unwrap(), c2.unwrap());
    println!("2 second session exporter differs: {}", code(&s2) != code(&s) && code(&s2) == code(&c2));

    // 3. Unknown client key: refused in the handshake.
    let (s3, c3) = handshake(&a, pin(&b), &stranger, pin(&a)).await;
    println!("3 unknown client: server {:?}; client {:?}", s3.err(), c3.err());

    // 4. Unknown server key: client refuses.
    let (s4, c4) = handshake(&stranger, pin(&b), &b, pin(&a)).await;
    println!("4 unknown server: server {:?}; client {:?}", s4.err(), c4.err());

    // 5. Pairing mode on both ends accepts strangers; codes agree.
    let (s5, c5) = handshake(&a, pairing(), &stranger, pairing()).await;
    let (s5, c5) = (s5.unwrap(), c5.unwrap());
    println!("5 pairing: ok, codes equal {}", code(&s5) == code(&c5));

    // 6. Loopback latency on one stream.
    latency(c, s).await;
}
