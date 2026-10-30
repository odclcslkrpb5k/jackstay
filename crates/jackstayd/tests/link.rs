//! Two daemons linked through the relay (support/link.rs) that can hurt the traffic between them
//! (docs/link.md, "Testing").

mod support;

use std::net::UdpSocket;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use jackstay_proto::control::{Dialer, LinkStatus, PairReason, PeerState, Request, Response};
use jackstayd::client::Client;
use support::link::{Faults, Machine, Relay, Rig, SOON, Toward, local, pair, ports};
use support::{Daemon, Home, Xvfb, ask, ok};

#[test]
fn two_daemons_pair_with_one_code_and_link() {
    let rig = Rig::new();
    let (on_a, on_b) = rig.pair();
    assert_eq!((&on_a.1, &on_b.1), (&Ok(()), &Ok(())));
    assert!(on_a.0.is_some() && on_a.0 == on_b.0, "{on_a:?} {on_b:?}");
    rig.a.peer_in(PeerState::Connected, SOON);
    let b = rig.b.wait(SOON, "a round trip", |l| l.peers.first().and_then(|p| p.rtt_us));
    assert!(b < 50_000, "{b} µs");
    // Each knows the other by its fingerprint, and lists itself by its own.
    let (la, lb) = (rig.a.link(), rig.b.link());
    // Both name the same machine as the one that dialed; a version 3 reply names neither.
    let dialed = (la.peers[0].dialed, lb.peers[0].dialed);
    assert!(
        matches!(
            dialed,
            (Some(Dialer::Us), Some(Dialer::Peer)) | (Some(Dialer::Peer), Some(Dialer::Us))
        ),
        "{dialed:?}"
    );
    let Response::Link(old) = ask(&rig.a.home, Request::Link) else { panic!("no link status") };
    assert_eq!(old.peers[0].dialed, None);
    assert_eq!(la.peers[0].fingerprint, lb.fingerprint);
    assert_eq!(lb.peers[0].fingerprint, la.fingerprint);
    assert_eq!(la.port, Some(rig.a.port));
}

#[test]
fn a_no_on_either_side_pairs_neither() {
    let rig = Rig::new();
    let on_a = pair(&rig.a, format!("127.0.0.1:{}", rig.relay.for_a), true);
    let on_b = pair(&rig.b, format!("127.0.0.1:{}", rig.relay.for_b), false);
    let (on_a, on_b) = (ok(on_a.join().map_err(|_| "a")), ok(on_b.join().map_err(|_| "b")));
    assert_eq!((on_a.1, on_b.1), (Err(PairReason::PeerEnded), Err(PairReason::Declined)));
    assert_eq!(rig.a.link().peers, Vec::new());
    assert_eq!(rig.b.link().peers, Vec::new());
}

#[test]
fn pairing_ends_when_its_client_leaves() {
    let rig = Rig::new();
    let mut c = ok(Client::connect(&rig.a.home.socket(), Duration::from_secs(10)));
    ok(c.send(&Request::Pair { host: format!("127.0.0.1:{}", rig.relay.for_a) }));
    assert_eq!(ok(c.receive()), Response::Ok);
    rig.a.wait(SOON, "pairing", |l| l.pairing.then_some(()));
    drop(c);
    rig.a.wait(SOON, "pairing to end", |l| (!l.pairing).then_some(()));
}

#[test]
fn a_second_pairing_client_is_told_pairing_is_busy() {
    let rig = Rig::new();
    let mut first = ok(Client::connect(&rig.a.home.socket(), Duration::from_secs(10)));
    ok(first.send(&Request::Pair { host: format!("127.0.0.1:{}", rig.relay.for_a) }));
    assert_eq!(ok(first.receive()), Response::Ok);
    let second = ask(&rig.a.home, Request::Pair { host: "127.0.0.1:1".into() });
    assert!(matches!(second, Response::PairFailed { reason: PairReason::Busy, .. }), "{second:?}");
    // And an answer from a client that is not pairing is refused.
    let answer = ask(&rig.a.home, Request::PairAnswer { yes: true });
    assert_eq!(answer, Response::Error(jackstay_proto::control::Error::NotPairing));
}

#[test]
fn unpairing_by_name_or_fingerprint() {
    let rig = Rig::linked();
    let b = rig.a.link().peers[0].clone();
    let unknown = ask(&rig.a.home, Request::Unpair { peer: "nobody".into() });
    assert_eq!(unknown, Response::Error(jackstay_proto::control::Error::NoSuchPeer));
    assert_eq!(ask(&rig.a.home, Request::Unpair { peer: b.fingerprint }), Response::Ok);
    assert_eq!(rig.a.link().peers, Vec::new());
    // b is told at once, and refused from then on.
    rig.b.peer_in(PeerState::RefusedKey, SOON);
}

#[test]
fn the_link_off_says_why_and_the_clipboard_runs_on() {
    let (x, home) = (Xvfb::start(), Home::new());
    ok(std::fs::write(home.config(), "[link]\nenabled = false\n"));
    let _d = Daemon::start(&x, &home);
    let Response::Link(l) = ask(&home, Request::Link) else { panic!("no link status") };
    assert_eq!(l.off.as_deref(), Some("disabled in the configuration (link.enabled)"));
    let refused = ask(&home, Request::Pair { host: "xiphos".into() });
    assert!(matches!(refused, Response::PairFailed { reason: PairReason::LinkOff, .. }));
    assert!(matches!(ask(&home, Request::Status), Response::Status(_)));
}

#[test]
fn a_black_hole_shorter_than_the_idle_timeout_drops_nothing() {
    let rig = Rig::linked();
    rig.relay.set(Faults { blackhole: true, ..Faults::default() });
    std::thread::sleep(Duration::from_secs(2));
    rig.relay.set(Faults::default());
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(rig.a.peer(), (PeerState::Connected, 1));
    assert_eq!(rig.b.peer(), (PeerState::Connected, 1));
}

#[test]
fn a_black_hole_longer_than_the_idle_timeout_is_recovered_from_soon_after() {
    let rig = Rig::linked();
    rig.relay.set(Faults { blackhole: true, ..Faults::default() });
    rig.a.peer_in(PeerState::Suspect, SOON);
    rig.a.peer_in(PeerState::Dialing, Duration::from_secs(10));
    std::thread::sleep(Duration::from_secs(2));
    rig.relay.set(Faults::default());
    let back = Instant::now();
    let a = rig.a.peer_in(PeerState::Connected, Duration::from_secs(6));
    rig.b.peer_in(PeerState::Connected, Duration::from_secs(2));
    assert!(back.elapsed() < Duration::from_secs(4), "{:?}", back.elapsed());
    assert_eq!(a, 2);
}

#[test]
fn loss_one_way_is_recovered_from_when_it_ends() {
    let rig = Rig::linked();
    // A hears nothing from B, while B still hears A: each side's view of the session differs.
    rig.relay.set(Faults { loss_to_a: 1.0, ..Faults::default() });
    rig.a.peer_in(PeerState::Dialing, Duration::from_secs(15));
    rig.relay.set(Faults::default());
    let back = Instant::now();
    rig.a.peer_in(PeerState::Connected, Duration::from_secs(8));
    rig.b.wait(Duration::from_secs(8), "B linked again", |l| {
        l.peers.first().filter(|p| p.state == PeerState::Connected && p.connects >= 2).map(|_| ())
    });
    assert!(back.elapsed() < Duration::from_secs(6), "{:?}", back.elapsed());
}

#[test]
fn steady_loss_and_delay_do_not_drop_the_link() {
    let rig = Rig::linked();
    rig.relay.set(Faults {
        loss_to_a: 0.05,
        loss_to_b: 0.05,
        delay: Duration::from_millis(2),
        ..Faults::default()
    });
    let until = Instant::now() + Duration::from_secs(8);
    let mut rtts = Vec::new();
    while Instant::now() < until {
        let l = rig.a.link();
        let p = &l.peers[0];
        assert_eq!((p.state, p.connects), (PeerState::Connected, 1), "{l:?}");
        rtts.extend(p.rtt_us);
        std::thread::sleep(Duration::from_millis(250));
    }
    // Two 2 ms delays each way, and seldom a loss: the typical round trip stays near 4 ms, and
    // none reaches human scale. A loss costs a probe timeout, doubling for each loss in a row, so
    // an outlier of a few hundred ms is the protocol working (275 ms seen once, under the whole
    // gate's load); seconds would mean recovery by something slower.
    assert!(!rtts.is_empty(), "no round trip measured");
    rtts.sort_unstable();
    let (median, worst) = (rtts[rtts.len() / 2], rtts.last().copied().unwrap_or(0));
    assert!((4_000..50_000).contains(&median), "{rtts:?}");
    assert!(worst < 1_000_000, "{rtts:?}");
}

#[test]
fn reordering_and_duplication_do_not_drop_the_link() {
    let rig = Rig::linked();
    rig.relay.set(Faults { reorder: true, duplicate: true, ..Faults::default() });
    std::thread::sleep(Duration::from_secs(4));
    rig.relay.set(Faults::default());
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(rig.a.peer(), (PeerState::Connected, 1));
    assert_eq!(rig.b.peer(), (PeerState::Connected, 1));
}

#[test]
fn a_new_source_port_for_the_dialing_machine_does_not_drop_the_link() {
    // The machine that dialed the kept session is QUIC's client, and a client may move (QUIC
    // migration). A server may not, and quinn drops a client's packets from an address it does not
    // know: a rebinding there is recovered only by dialing again (docs/link.md, "Testing"). Asked,
    // not taken from the keys: the smaller key's session usually comes up alone, but not always.
    let rig = Rig::linked();
    let dialer = match rig.a.link().peers[0].dialed {
        Some(Dialer::Us) => Toward::A,
        Some(Dialer::Peer) => Toward::B,
        None => panic!("no kept session"),
    };
    rig.relay.rebind(dialer);
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(rig.a.peer(), (PeerState::Connected, 1));
    assert_eq!(rig.b.peer(), (PeerState::Connected, 1));
}

#[test]
fn a_path_that_drops_large_datagrams_keeps_the_link() {
    let rig = Rig::linked();
    rig.relay.set(Faults { largest: Some(1280), ..Faults::default() });
    std::thread::sleep(Duration::from_secs(5));
    assert_eq!(rig.a.peer(), (PeerState::Connected, 1));
    assert_eq!(rig.b.peer(), (PeerState::Connected, 1));
}

/// Kills `killed`'s daemon without a goodbye and starts it again; `other` must link with it
/// again within 3 s.
fn kill_and_restart(killed: &mut Machine, other: &Machine) {
    let before = other.peer().1;
    killed.signal("-KILL");
    if let Some(mut d) = killed.daemon.take() {
        let _ = d.child.wait();
    }
    killed.daemon = Some(Daemon::start(&killed.x, &killed.home));
    let back = Instant::now();
    other.wait(SOON, "a new session", |l| {
        l.peers
            .first()
            .filter(|p| p.state == PeerState::Connected && p.connects > before)
            .map(|_| ())
    });
    assert!(back.elapsed() < Duration::from_secs(3), "{:?}", back.elapsed());
}

#[test]
fn a_restarted_daemon_is_linked_again_at_once() {
    let mut rig = Rig::linked();
    // Stopped as systemd stops it (goodbyes said), then killed without any, each machine in turn.
    rig.b.restart();
    let back = Instant::now();
    rig.a.wait(SOON, "a new session", |l| {
        l.peers.first().filter(|p| p.state == PeerState::Connected && p.connects >= 2).map(|_| ())
    });
    rig.b.peer_in(PeerState::Connected, SOON);
    assert!(back.elapsed() < Duration::from_secs(3), "{:?}", back.elapsed());
    kill_and_restart(&mut rig.a, &rig.b);
    kill_and_restart(&mut rig.b, &rig.a);
}

#[test]
fn a_daemon_stopped_as_by_suspend_is_linked_again_when_it_continues() {
    let mut rig = Rig::linked();
    rig.b.signal("-STOP");
    rig.a.peer_in(PeerState::Suspect, SOON);
    rig.a.peer_in(PeerState::Dialing, Duration::from_secs(10));
    // 30 s in all: well past the link thread's 10 s of allowed silence, which a whole process
    // paused must not count against it.
    std::thread::sleep(Duration::from_secs(17));
    rig.b.signal("-CONT");
    let back = Instant::now();
    rig.a.peer_in(PeerState::Connected, Duration::from_secs(6));
    rig.b.peer_in(PeerState::Connected, Duration::from_secs(6));
    assert!(back.elapsed() < Duration::from_secs(5), "{:?}", back.elapsed());
    let d = rig.b.daemon.as_mut().unwrap_or_else(|| panic!("not running"));
    assert_eq!(ok(d.child.try_wait()), None, "B's daemon exited after the pause");
}

/// A verifier for the stand-ins that accepts any raw Ed25519 key, as either end: they play
/// machines that are not ours.
#[derive(Debug)]
struct AnyKey;

impl rustls::client::danger::ServerCertVerifier for AnyKey {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![rustls::SignatureScheme::ED25519]
    }
    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

impl rustls::server::danger::ClientCertVerifier for AnyKey {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        Ok(rustls::server::danger::ClientCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![rustls::SignatureScheme::ED25519]
    }
    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A key pair for a stand-in, larger than every key in `above` and smaller than every key in
/// `below`, and its public key as it travels.
fn stand_in_key(
    above: &[Vec<u8>],
    below: &[Vec<u8>],
) -> (Arc<rustls::sign::CertifiedKey>, Vec<u8>) {
    loop {
        let pkcs8 =
            ok(ring::signature::Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new())
                .map_err(|_| "no key"));
        let der = rustls::pki_types::PrivatePkcs8KeyDer::from(pkcs8.as_ref().to_vec());
        let signer = ok(rustls::crypto::ring::sign::any_eddsa_type(&der));
        let spki = signer.public_key().unwrap_or_else(|| panic!("no spki")).as_ref().to_vec();
        if above.iter().all(|k| spki > *k) && below.iter().all(|k| spki < *k) {
            let cert = rustls::pki_types::CertificateDer::from(spki.clone());
            return (Arc::new(rustls::sign::CertifiedKey::new(vec![cert], signer)), spki);
        }
    }
}

/// A machine's public key, read from its identity file.
fn public_key(m: &Machine) -> Vec<u8> {
    let pkcs8 = ok(std::fs::read(m.home.state().join("identity")));
    let der = rustls::pki_types::PrivatePkcs8KeyDer::from(pkcs8);
    let signer = ok(rustls::crypto::ring::sign::any_eddsa_type(&der));
    signer.public_key().unwrap_or_else(|| panic!("no spki")).as_ref().to_vec()
}

fn client_config(key: &Arc<rustls::sign::CertifiedKey>, alpn: &[u8]) -> quinn::ClientConfig {
    let mut tls = ok(rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13]))
    .dangerous()
    .with_custom_certificate_verifier(Arc::new(AnyKey))
    .with_client_cert_resolver(Arc::new(rustls::client::AlwaysResolvesClientRawPublicKeys::new(
        Arc::clone(key),
    )));
    tls.alpn_protocols = vec![alpn.to_vec()];
    quinn::ClientConfig::new(Arc::new(ok(quinn::crypto::rustls::QuicClientConfig::try_from(tls))))
}

fn server_config(key: &Arc<rustls::sign::CertifiedKey>, alpn: &[u8]) -> quinn::ServerConfig {
    let mut tls = ok(rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13]))
    .with_client_cert_verifier(Arc::new(AnyKey))
    .with_cert_resolver(Arc::new(rustls::server::AlwaysResolvesServerRawPublicKeys::new(
        Arc::clone(key),
    )));
    tls.alpn_protocols = vec![alpn.to_vec()];
    quinn::ServerConfig::with_crypto(Arc::new(ok(
        quinn::crypto::rustls::QuicServerConfig::try_from(tls),
    )))
}

/// A machine in the middle of pairing: it answers A's pairing dial at `for_a`, dials B's pairing
/// port as A would, and copies the bytes of the one stream each way, unchanged. Its key lies
/// between the machines' (`keys`, smaller first): the smaller machine commits first, the larger
/// follows, and both put the two nonces in the same order. So only what the code takes from each
/// TLS session tells the two codes apart.
fn man_in_the_middle(for_a: u16, b: u16, keys: &[Vec<u8>; 2]) -> JoinHandle<()> {
    let (key, _) = stand_in_key(&keys[..1], &keys[1..]);
    std::thread::spawn(move || {
        let rt = ok(tokio::runtime::Builder::new_current_thread().enable_all().build());
        rt.block_on(async move {
            let pair = jackstay_net::PAIR_ALPN;
            let server = ok(quinn::Endpoint::server(server_config(&key, pair), local(for_a)));
            let mut client = ok(quinn::Endpoint::client(local(0)));
            client.set_default_client_config(client_config(&key, pair));
            let Some(incoming) = server.accept().await else { return };
            let Ok(to_a) = incoming.await else { return };
            let Ok(Ok(to_b)) = tokio::time::timeout(
                Duration::from_secs(5),
                ok(client.connect(local(b), "jackstay")),
            )
            .await
            else {
                return;
            };
            // A dialed, so A opens the stream; to B, the stand-in is the dialer and opens it.
            let Ok((mut a_send, mut a_recv)) = to_a.accept_bi().await else { return };
            let Ok((mut b_send, mut b_recv)) = to_b.open_bi().await else { return };
            let a_to_b = async move {
                let mut buf = [0u8; 4096];
                while let Ok(Some(n)) = a_recv.read(&mut buf).await {
                    if b_send.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            };
            let b_to_a = async move {
                let mut buf = [0u8; 4096];
                while let Ok(Some(n)) = b_recv.read(&mut buf).await {
                    if a_send.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            };
            let both = Box::pin(async { tokio::join!(a_to_b, b_to_a) });
            let _ = tokio::time::timeout(Duration::from_secs(30), both).await;
        });
    })
}

#[test]
fn a_machine_in_the_middle_of_pairing_cannot_make_the_codes_match() {
    let (a, b) = (Machine::new(), Machine::new());
    let relay = Relay::with(a.port, b.port, false);
    let mut keys = [public_key(&a), public_key(&b)];
    keys.sort();
    let mitm = man_in_the_middle(relay.for_a + 1, b.port + 1, &keys);
    // Careless users who say yes without comparing: the codes still differ.
    let on_a = pair(&a, format!("127.0.0.1:{}", relay.for_a), true);
    let on_b = pair(&b, format!("127.0.0.1:{}", relay.for_b), true);
    let (on_a, on_b) = (ok(on_a.join().map_err(|_| "a")), ok(on_b.join().map_err(|_| "b")));
    let (code_a, code_b) = (on_a.0.clone().unwrap_or_default(), on_b.0.clone().unwrap_or_default());
    assert!(!code_a.is_empty() && !code_b.is_empty(), "{on_a:?} {on_b:?}");
    // One chance in a million of a false failure.
    assert_ne!(code_a, code_b);
    let _ = mitm.join();
}

/// The first datagram of a handshake toward our link, as any machine on the network could send it.
fn an_initial() -> Vec<u8> {
    let hole = ok(UdpSocket::bind(local(0)));
    let to = ok(hole.local_addr());
    let (key, _) = stand_in_key(&[], &[]);
    let rt = ok(tokio::runtime::Builder::new_current_thread().enable_all().build());
    let _guard = rt.enter();
    let mut client = ok(quinn::Endpoint::client(local(0)));
    client.set_default_client_config(client_config(&key, jackstay_net::ALPN));
    let connecting = ok(client.connect(to, "jackstay"));
    let mut buf = vec![0u8; 2048];
    ok(hole.set_read_timeout(Some(Duration::from_secs(5))));
    let reader = std::thread::spawn(move || {
        let n = ok(hole.recv(&mut buf));
        buf.truncate(n);
        buf
    });
    // Drive the client until its first datagram has gone.
    rt.block_on(async {
        let _ = tokio::time::timeout(Duration::from_millis(300), connecting).await;
    });
    ok(reader.join().map_err(|_| "reader"))
}

#[test]
fn forged_handshakes_do_not_keep_a_peer_out() {
    let mut rig = Rig::linked();
    // A can no longer dial B (a wrong address in its peers file): only B's dials link them, and
    // they must get through A's handshake limit while the flood runs.
    rig.a.stop();
    let peers = rig.a.home.state().join("peers");
    let text = ok(std::fs::read_to_string(&peers));
    let wrong: String = text
        .lines()
        .map(|l| {
            let f: Vec<&str> = l.splitn(4, ' ').collect();
            if f.first() == Some(&"peer") && f.len() == 4 {
                format!("peer {} 127.0.0.1:1 {}\n", f[1], f[3])
            } else {
                format!("{l}\n")
            }
        })
        .collect();
    ok(std::fs::write(&peers, wrong));
    // 500 a second, each from a port of its own that never answers: forged sources.
    let initial = an_initial();
    let target = local(rig.a.port);
    let stop = Arc::new(AtomicBool::new(false));
    let flooding = Arc::clone(&stop);
    let flood = std::thread::spawn(move || {
        let mut sent = 0u32;
        while !flooding.load(Ordering::Relaxed) {
            if let Ok(s) = UdpSocket::bind(local(0)) {
                let _ = s.send_to(&initial, target);
                sent += 1;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        sent
    });
    std::thread::sleep(Duration::from_millis(500));
    rig.a.daemon = Some(Daemon::start(&rig.a.x, &rig.a.home));
    let started = Instant::now();
    rig.a.peer_in(PeerState::Connected, Duration::from_secs(8));
    let took = started.elapsed();
    stop.store(true, Ordering::Relaxed);
    let sent = ok(flood.join().map_err(|_| "flood"));
    // Started 0.5 s before A: at least 100 a second for the whole time.
    let rate = f64::from(sent) / (took.as_secs_f64() + 0.5);
    assert!(rate > 100.0, "only {sent} forged handshakes in {took:?}");
    assert!(took < Duration::from_secs(5), "{took:?}");
}

#[test]
fn round_trips_stay_short_while_forged_handshakes_flood_in() {
    let rig = Rig::linked();
    let initial = an_initial();
    let (a, b) = (local(rig.a.port), local(rig.b.port));
    let stop = Arc::new(AtomicBool::new(false));
    let flooding = Arc::clone(&stop);
    let flood = std::thread::spawn(move || {
        while !flooding.load(Ordering::Relaxed) {
            for target in [a, b] {
                if let Ok(s) = UdpSocket::bind(local(0)) {
                    let _ = s.send_to(&initial, target);
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    });
    let until = Instant::now() + Duration::from_secs(5);
    let mut rtts = Vec::new();
    while Instant::now() < until {
        let l = rig.a.link();
        let p = &l.peers[0];
        assert_eq!((p.state, p.connects), (PeerState::Connected, 1), "{l:?}");
        rtts.extend(p.rtt_us);
        std::thread::sleep(Duration::from_millis(250));
    }
    stop.store(true, Ordering::Relaxed);
    let _ = flood.join();
    let worst = rtts.iter().max().copied().unwrap_or(u32::MAX);
    assert!(worst < 50_000, "{rtts:?}");
}

/// The link's status once `ready` holds of it, within 5 s.
fn wait_link(home: &Home, ready: impl Fn(&LinkStatus) -> bool) -> Response {
    let until = Instant::now() + SOON;
    loop {
        let r = ask(home, Request::Link);
        if matches!(&r, Response::Link(l) if ready(l)) || Instant::now() >= until {
            return r;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_link_that_keeps_failing_starts_off_once() {
    let (x, home) = (Xvfb::start(), Home::new());
    ok(std::fs::write(home.config(), format!("[link]\nport = {}\n", ports())));
    ok(std::fs::create_dir_all(home.state()));
    ok(std::fs::write(home.state().join("link-failures"), "3\n"));
    let d = Daemon::start(&x, &home);
    let Response::Link(l) = ask(&home, Request::Link) else { panic!("no link status") };
    assert_eq!(l.off.as_deref(), Some("it failed 3 times in a row; off until jackstayd restarts"));
    // Off for this run only: the next start tries again.
    assert!(!home.state().join("link-failures").exists());
    let _ = d.stop();
    let _d = Daemon::start(&x, &home);
    let Response::Link(l) = ask(&home, Request::Link) else { panic!("no link status") };
    assert_eq!(l.off, None, "{l:?}");
    let Response::Link(l) = wait_link(&home, |l| l.port.is_some()) else { panic!("no port") };
    assert_eq!(l.off, None);
}

#[test]
fn a_host_that_is_not_one_is_named_as_such() {
    let rig = Rig::new();
    let r = ask(&rig.a.home, Request::Pair { host: "not a host".into() });
    assert!(matches!(r, Response::PairFailed { reason: PairReason::BadAddress, .. }), "{r:?}");
    assert!(!rig.a.link().pairing);
}
