//! Direct (peer-to-peer) QUIC path between viewer and helper.
//!
//! Every session starts through the relay. Then both sides:
//! 1. bind one UDP socket ([`DirectEndpoint::bind`]), ask the relay's STUN
//!    answerer ([`crate::stun`]) how the internet sees it, and make a
//!    throw-away certificate for this session;
//! 2. tell each other their addresses and certificate fingerprint through the
//!    relay (`SessionMsg::DirectOffer`: the relay has already checked who may
//!    connect, so pinning these fingerprints is what admits the peer);
//! 3. connect to all of the other side's addresses at once, both acting as
//!    QUIC client *and* server: the outgoing packets open each side's NAT, and
//!    whichever connection comes up first is used ([`DirectEndpoint::establish`]);
//!    the viewer picks by opening the one stream on it.
//!
//! The stream then carries the same messages as the relay's WebSocket
//! ([`LinkTx::send`] / [`LinkRx::recv`]); see `host::run_session` and
//! `net::network_task` for switching between the two paths.

use crate::proto::SessionMsg;
use anyhow::{Context, Result, anyhow, bail};
use rustls::DigitallySignedStruct;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use sha2::{Digest, Sha256};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message;

const ALPN: &[u8] = b"farlink-direct/1";
const SERVER_NAME: &str = "farlink";
/// How long both sides try before staying on the relay.
pub const ESTABLISH_TIMEOUT: Duration = Duration::from_secs(5);
/// Most addresses taken from the other side.
pub const MAX_ADDRS: usize = 8;
/// No sign of life from the other side for this long: the direct
/// connection counts as broken and everything goes through the relay again.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(4);
/// While a session runs, the relay socket is pinged this often: once the
/// pictures go directly it carries almost nothing, and proxies drop idle
/// connections, which would end the session.
pub const RELAY_PING: Duration = Duration::from_secs(15);
/// Most extra STUN servers asked.
pub const MAX_EXTRA_STUN: usize = 4;
const KIND_START: u8 = 0;
const KIND_TEXT: u8 = 1;
const KIND_BINARY: u8 = 2;
const MAX_FRAME: usize = 32 << 20;

/// This side's UDP socket, certificate and the addresses others can try.
pub struct DirectEndpoint {
    endpoint: quinn::Endpoint,
    cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
    /// SHA-256 of the certificate (hex), sent to the other side.
    pub fingerprint: String,
    /// LAN address, then the address the relay saw (same port; left out
    /// behind a hard NAT).
    pub addrs: Vec<SocketAddr>,
    /// The NAT in front of this computer gives every destination its own
    /// port (see [`crate::stun::hard_nat`]): only a LAN path can work.
    pub hard_nat: bool,
}

/// A working direct connection.
pub struct Link {
    pub tx: LinkTx,
    pub rx: LinkRx,
    /// The other side's address.
    pub remote: SocketAddr,
}

pub struct LinkTx {
    send: quinn::SendStream,
    conn: quinn::Connection,
    /// Keeps the socket open as long as the link lives.
    _endpoint: quinn::Endpoint,
}

/// Messages from the other side, whole. A task of its own reads the
/// stream: waiting here can be interrupted (`select!`) without losing part
/// of a message, which reading the stream directly can't.
pub struct LinkRx {
    messages: tokio::sync::mpsc::Receiver<Result<Message>>,
    reader: tokio::task::JoinHandle<()>,
}

impl Drop for LinkRx {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

pub fn fingerprint(cert: &[u8]) -> String {
    hex::encode(Sha256::digest(cert))
}

/// Is this address inside a local network (no NAT crossed)?
pub fn is_lan(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_private() || v4.is_loopback() || v4.is_link_local(),
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
        }
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn transport() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    // A dead path is noticed within a few seconds; then the relay takes over.
    t.keep_alive_interval(Some(Duration::from_secs(1)));
    t.max_idle_timeout(Some(IDLE_TIMEOUT.try_into().expect("small timeout")));
    // Whole pictures in flight on fast links.
    t.stream_receive_window(quinn::VarInt::from_u32(16 << 20));
    t.receive_window(quinn::VarInt::from_u32(32 << 20));
    t.send_window(32 << 20);
    Arc::new(t)
}

/// Accepts exactly one certificate: the one the relay told us about.
#[derive(Debug)]
struct Pinned {
    fingerprint: String,
    provider: Arc<CryptoProvider>,
}

impl Pinned {
    fn check(&self, cert: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        if crate::util::constant_time_eq(&fingerprint(cert), &self.fingerprint) {
            Ok(())
        } else {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn tls12(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            m,
            c,
            d,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn tls13(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            m,
            c,
            d,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|_| ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls12(m, c, d)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls13(m, c, d)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.schemes()
    }
}

impl ClientCertVerifier for Pinned {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|_| ClientCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls12(m, c, d)
    }
    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.tls13(m, c, d)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.schemes()
    }
}

/// The relay's host name (or address) in a server URL.
pub fn relay_host(server: &str) -> Option<String> {
    let rest = server
        .trim()
        .split_once("://")
        .map_or(server.trim(), |(_, r)| r);
    let authority = rest.split(['/', '?']).next()?;
    let host = if let Some(v6) = authority.strip_prefix('[') {
        v6.split(']').next()?
    } else {
        authority.rsplit_once(':').map_or(authority, |(h, _)| h)
    };
    (!host.is_empty()).then(|| host.to_string())
}

/// The addresses to offer, and whether the NAT is hard, from this
/// socket's LAN address and what the STUN servers saw. Only answers from
/// across the internet count for the NAT check: a relay on the same
/// network sees the LAN address, which says nothing about the NAT.
pub fn candidates(local: Option<SocketAddr>, seen: &[SocketAddr]) -> (Vec<SocketAddr>, bool) {
    let public: Vec<SocketAddr> = seen.iter().copied().filter(|a| !is_lan(a.ip())).collect();
    let hard_nat = crate::stun::hard_nat(&public);
    let mut addrs: Vec<SocketAddr> = Vec::new();
    for a in local.into_iter().chain(seen.iter().copied()) {
        if (!hard_nat || is_lan(a.ip())) && !addrs.contains(&a) {
            addrs.push(a);
        }
    }
    (addrs, hard_nat)
}

impl DirectEndpoint {
    /// Bind a socket and learn its addresses from the relay's STUN answerer
    /// on `relay_host`:`stun_port` (IPv4), and from `extra` STUN servers
    /// (`host:port`, from the relay's settings) to tell a hard NAT.
    pub async fn bind(
        relay_host: &str,
        stun_port: u16,
        extra: &[String],
    ) -> Result<DirectEndpoint> {
        let stun = tokio::net::lookup_host((relay_host, stun_port))
            .await
            .context("looking up the relay")?
            .find(SocketAddr::is_ipv4)
            .ok_or_else(|| anyhow!("the relay has no IPv4 address"))?;
        let mut servers = vec![stun];
        for name in extra.iter().take(MAX_EXTRA_STUN) {
            let found = tokio::time::timeout(
                Duration::from_secs(1),
                tokio::net::lookup_host(name.as_str()),
            )
            .await;
            if let Ok(Ok(mut found)) = found
                && let Some(addr) = found.find(SocketAddr::is_ipv4)
                && !servers.contains(&addr)
            {
                servers.push(addr);
            }
        }
        let sock = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
        let port = sock.local_addr()?.port();
        let answers = crate::stun::query_many(&sock, &servers, Duration::from_millis(1500)).await;
        let seen: Vec<SocketAddr> = answers.iter().flatten().copied().collect();
        // The address this computer uses towards the relay = its LAN address.
        let local = std::net::UdpSocket::bind("0.0.0.0:0")
            .and_then(|probe| probe.connect(stun).and_then(|_| probe.local_addr()))
            .ok()
            .filter(|l| !l.ip().is_unspecified())
            .map(|l| SocketAddr::new(l.ip(), port));
        let (addrs, hard_nat) = candidates(local, &seen);
        if hard_nat {
            log::info!("direct connection: hard NAT here (seen as {seen:?}), LAN only");
        }
        if addrs.is_empty() {
            bail!("no address to offer");
        }
        let mut ep = Self::with_socket(sock.into_std()?, addrs)?;
        ep.hard_nat = hard_nat;
        Ok(ep)
    }

    /// An endpoint on `sock` offering `addrs` (tests: loopback only).
    pub fn with_socket(
        sock: std::net::UdpSocket,
        addrs: Vec<SocketAddr>,
    ) -> Result<DirectEndpoint> {
        let made = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_string()])?;
        let cert = made.cert.der().clone();
        let key = PrivatePkcs8KeyDer::from(made.key_pair.serialize_der());
        let endpoint = quinn::Endpoint::new(
            quinn::EndpointConfig::default(),
            None,
            sock,
            Arc::new(quinn::TokioRuntime),
        )?;
        Ok(DirectEndpoint {
            fingerprint: fingerprint(&cert),
            endpoint,
            cert,
            key,
            addrs,
            hard_nat: false,
        })
    }

    fn server_config(&self, peer: &str) -> Result<quinn::ServerConfig> {
        let pinned = Arc::new(Pinned {
            fingerprint: peer.to_string(),
            provider: provider(),
        });
        let mut tls = rustls::ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .with_client_cert_verifier(pinned)
            .with_single_cert(
                vec![self.cert.clone()],
                PrivateKeyDer::Pkcs8(self.key.clone_key()),
            )?;
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(tls)?,
        ));
        cfg.transport_config(transport());
        Ok(cfg)
    }

    fn client_config(&self, peer: &str) -> Result<quinn::ClientConfig> {
        let pinned = Arc::new(Pinned {
            fingerprint: peer.to_string(),
            provider: provider(),
        });
        let mut tls = rustls::ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .dangerous()
            .with_custom_certificate_verifier(pinned)
            .with_client_auth_cert(
                vec![self.cert.clone()],
                PrivateKeyDer::Pkcs8(self.key.clone_key()),
            )?;
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let mut cfg = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(tls)?,
        ));
        cfg.transport_config(transport());
        Ok(cfg)
    }

    /// Connect to the peer (fingerprint `peer_fp`, addresses `peer_addrs`).
    /// `viewer`: this side chooses the connection. `None` after `timeout`.
    pub async fn establish(
        self,
        viewer: bool,
        peer_fp: &str,
        peer_addrs: &[SocketAddr],
        timeout: Duration,
    ) -> Option<Link> {
        match tokio::time::timeout(timeout, self.race(viewer, peer_fp, peer_addrs)).await {
            Ok(Ok(link)) => Some(link),
            Ok(Err(e)) => {
                log::info!("direct connection: {e:#}");
                None
            }
            Err(_) => {
                log::info!("direct connection: no answer, staying on the relay");
                None
            }
        }
    }

    async fn race(self, viewer: bool, peer_fp: &str, peer_addrs: &[SocketAddr]) -> Result<Link> {
        let ep = self.endpoint.clone();
        ep.set_server_config(Some(self.server_config(peer_fp)?));
        let client = self.client_config(peer_fp)?;
        let (conn_tx, mut conn_rx) = tokio::sync::mpsc::unbounded_channel::<quinn::Connection>();
        let mut tasks = tokio::task::JoinSet::new();
        for &addr in peer_addrs.iter().filter(|a| a.is_ipv4()).take(MAX_ADDRS) {
            let Ok(connecting) = ep.connect_with(client.clone(), addr, SERVER_NAME) else {
                continue;
            };
            let tx = conn_tx.clone();
            tasks.spawn(async move {
                if let Ok(c) = connecting.await {
                    let _ = tx.send(c);
                }
            });
        }
        {
            let (ep, tx) = (ep.clone(), conn_tx.clone());
            tasks.spawn(async move {
                while let Some(incoming) = ep.accept().await {
                    let tx = tx.clone();
                    tokio::spawn(async move {
                        if let Ok(c) = incoming.await {
                            let _ = tx.send(c);
                        }
                    });
                }
            });
        }
        drop(conn_tx);
        let (conn, send, recv) = if viewer {
            let conn = conn_rx
                .recv()
                .await
                .ok_or_else(|| anyhow!("no connection"))?;
            let (mut send, mut recv) = conn.open_bi().await?;
            // A stream only shows up on the other side once something is written.
            send.write_all(&[KIND_START, 0, 0, 0, 0]).await?;
            // With TLS 1.3 a client counts the handshake done before the server
            // has checked the client's certificate: only the host's answer
            // proves it took this connection.
            let mut answer = [0u8; 5];
            recv.read_exact(&mut answer).await?;
            if answer[0] != KIND_START {
                bail!("unexpected answer");
            }
            (conn, send, recv)
        } else {
            // Wait for the stream the viewer opens, on whichever connection.
            let mut streams = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    c = conn_rx.recv() => {
                        let c = c.ok_or_else(|| anyhow!("no connection"))?;
                        streams.spawn(async move { c.accept_bi().await.map(|(s, r)| (c, s, r)) });
                    }
                    Some(Ok(Ok((c, mut s, mut r)))) = streams.join_next() => {
                        let mut start = [0u8; 5];
                        r.read_exact(&mut start).await?;
                        if start[0] != KIND_START { bail!("unexpected first frame"); }
                        s.write_all(&[KIND_START, 0, 0, 0, 0]).await?;
                        break (c, s, r);
                    }
                }
            }
        };
        tasks.abort_all();
        ep.set_server_config(None); // no more connections on this socket
        let remote = conn.remote_address();
        Ok(Link {
            tx: LinkTx {
                send,
                conn,
                _endpoint: ep,
            },
            rx: LinkRx::start(recv),
            remote,
        })
    }
}

impl LinkTx {
    pub async fn send(&mut self, msg: &Message) -> Result<()> {
        let (kind, data): (u8, &[u8]) = match msg {
            Message::Text(t) => (KIND_TEXT, t.as_bytes()),
            Message::Binary(b) => (KIND_BINARY, b),
            _ => return Ok(()),
        };
        let mut head = [kind, 0, 0, 0, 0];
        head[1..].copy_from_slice(&(data.len() as u32).to_le_bytes());
        self.send.write_all(&head).await?;
        self.send.write_all(data).await?;
        Ok(())
    }

    /// End the connection (the other side sees it closed at once).
    pub fn close(&self) {
        self.conn.close(0u32.into(), b"bye");
    }
}

impl LinkRx {
    fn start(mut recv: quinn::RecvStream) -> LinkRx {
        // A few pictures may wait here; beyond that, QUIC holds the sender back.
        let (tx, messages) = tokio::sync::mpsc::channel(16);
        let reader = tokio::spawn(async move {
            loop {
                let m = read_frame(&mut recv).await;
                let failed = m.is_err();
                if tx.send(m).await.is_err() || failed {
                    return;
                }
            }
        });
        LinkRx { messages, reader }
    }

    /// The next message (safe to interrupt). An error: the link broke.
    pub async fn recv(&mut self) -> Result<Message> {
        self.messages
            .recv()
            .await
            .unwrap_or_else(|| Err(anyhow!("direct connection closed")))
    }
}

async fn read_frame(recv: &mut quinn::RecvStream) -> Result<Message> {
    let mut head = [0u8; 5];
    recv.read_exact(&mut head).await?;
    let len = u32::from_le_bytes(head[1..].try_into().unwrap()) as usize;
    if len > MAX_FRAME {
        bail!("frame too big");
    }
    let mut data = vec![0u8; len];
    recv.read_exact(&mut data).await?;
    match head[0] {
        KIND_TEXT => Ok(Message::Text(String::from_utf8(data)?.into())),
        KIND_BINARY => Ok(Message::Binary(data.into())),
        k => bail!("unknown frame {k}"),
    }
}

/// Things that finish in the background while a session runs, each for
/// one attempt (stale ones are ignored).
pub enum Event {
    Bound(u32, Result<DirectEndpoint>),
    Linked(u32, Option<Link>),
    /// Time to try again (viewer only).
    Retry(u32),
    /// We offered `ESTABLISH_TIMEOUT` ago: has the other side answered?
    NoAnswer(u32),
}

/// After a failed attempt or a broken connection the viewer tries again
/// this long later (the last one repeats): the network may have changed
/// (Wi-Fi back, other network, router restarted).
pub const RETRY_AFTER: [Duration; 3] = [
    Duration::from_secs(5),
    Duration::from_secs(30),
    Duration::from_secs(120),
];

/// One side's direct-path state during a session. The relay socket stays
/// the authority (mode switches, end of session) and the fallback: the
/// caller sends every `SessionMsg` this returns on the relay, in order.
///
/// Attempts are numbered (`DirectOffer.attempt`). The first one starts on
/// both sides with `DirectInfo`; later ones only the viewer starts (after
/// [`RETRY_AFTER`]); the host joins when an offer with a newer number arrives.
pub struct DirectState {
    viewer: bool,
    events: tokio::sync::mpsc::UnboundedSender<Event>,
    /// The relay said direct connections are allowed (only then is anything
    /// tried): where to ask for our address.
    stun: Option<(String, u16, Vec<String>)>,
    attempt: u32,
    /// Attempts in a row that came to nothing (picks the retry delay).
    failures: u32,
    retry_after: Vec<Duration>,
    mine: Option<DirectEndpoint>,
    peer: Option<(String, Vec<SocketAddr>)>,
    tx: Option<LinkTx>,
    rx: Option<LinkRx>,
    remote: Option<SocketAddr>,
    /// This side sends on the direct path.
    sending: bool,
    /// The other side said its messages now come on the direct path.
    reading: bool,
    /// Why there is no direct path (for the viewer to show).
    why_not: Option<&'static str>,
}

const LOOKING: &str = "looking for a direct path";

impl DirectState {
    pub fn new(viewer: bool) -> (DirectState, tokio::sync::mpsc::UnboundedReceiver<Event>) {
        let (events, rx) = tokio::sync::mpsc::unbounded_channel();
        let s = DirectState {
            viewer,
            events,
            stun: None,
            attempt: 0,
            failures: 0,
            retry_after: RETRY_AFTER.to_vec(),
            mine: None,
            peer: None,
            tx: None,
            rx: None,
            remote: None,
            sending: false,
            reading: false,
            why_not: Some("the server allows no direct connections"),
        };
        (s, rx)
    }

    /// The relay allows direct connections (`DirectInfo`): bind and ask
    /// its STUN answerer (and `extra` servers), in the background
    /// (→ [`Event::Bound`]).
    pub fn allowed(&mut self, server: &str, stun_port: u16, extra: Vec<String>) {
        if self.stun.is_some() || stun_port == 0 {
            return;
        }
        let Some(host) = relay_host(server) else {
            return;
        };
        self.stun = Some((host, stun_port, extra));
        self.why_not = Some(LOOKING);
        self.bind();
    }

    fn bind(&mut self) {
        let Some((host, port, extra)) = self.stun.clone() else {
            return;
        };
        let (events, attempt) = (self.events.clone(), self.attempt);
        tokio::spawn(async move {
            let _ = events.send(Event::Bound(
                attempt,
                DirectEndpoint::bind(&host, port, &extra).await,
            ));
        });
    }

    /// Our socket for `attempt` is ready: the offer to send to the other side.
    pub fn bound(&mut self, attempt: u32, ep: Result<DirectEndpoint>) -> Option<SessionMsg> {
        if attempt != self.attempt {
            return None;
        }
        let ep = match ep {
            Ok(ep) => ep,
            Err(e) => {
                log::info!("direct connection: {e:#}");
                self.why_not = Some("no UDP connection to the server");
                self.schedule_retry();
                return None;
            }
        };
        if ep.hard_nat {
            self.why_not = Some(if self.viewer {
                "this computer is behind a strict NAT"
            } else {
                "the remote computer is behind a strict NAT"
            });
        }
        let offer = SessionMsg::DirectOffer {
            fp: ep.fingerprint.clone(),
            addrs: ep.addrs.clone(),
            hard_nat: ep.hard_nat,
            attempt,
        };
        self.mine = Some(ep);
        if self.viewer {
            let events = self.events.clone();
            tokio::spawn(async move {
                tokio::time::sleep(ESTABLISH_TIMEOUT).await;
                let _ = events.send(Event::NoAnswer(attempt));
            });
        }
        self.try_establish();
        Some(offer)
    }

    /// The other side's offer arrived (through the relay). A newer attempt
    /// than ours (the viewer is trying again) ends whatever we had: returns
    /// the switch back to the relay to send, if we were still sending directly.
    pub fn offer(
        &mut self,
        fp: String,
        mut addrs: Vec<SocketAddr>,
        hard_nat: bool,
        attempt: u32,
    ) -> Option<SessionMsg> {
        if self.stun.is_none() || attempt < self.attempt || (attempt > self.attempt && self.viewer)
        {
            return None; // not allowed, stale, or not ours to start
        }
        let mut back = None;
        if attempt > self.attempt {
            back = self.broken();
            self.attempt = attempt;
            self.mine = None;
            self.peer = None;
            self.bind();
        }
        addrs.truncate(MAX_ADDRS);
        if hard_nat {
            self.why_not = Some(if self.viewer {
                "the remote computer is behind a strict NAT"
            } else {
                "the viewer is behind a strict NAT"
            });
        }
        if self.peer.is_none() && !fp.is_empty() && !addrs.is_empty() {
            self.peer = Some((fp, addrs));
            self.try_establish();
        }
        back
    }

    /// Why the session is not on a direct path (`None` while it is).
    pub fn why_not(&self) -> Option<&'static str> {
        if self.remote().is_some() {
            None
        } else {
            self.why_not
        }
    }

    fn try_establish(&mut self) {
        if self.mine.is_none() || self.peer.is_none() {
            return;
        }
        let (ep, (fp, addrs)) = (self.mine.take().unwrap(), self.peer.clone().unwrap());
        let (events, viewer, attempt) = (self.events.clone(), self.viewer, self.attempt);
        tokio::spawn(async move {
            let link = ep.establish(viewer, &fp, &addrs, ESTABLISH_TIMEOUT).await;
            let _ = events.send(Event::Linked(attempt, link));
        });
    }

    /// The viewer tries again later (the host follows its offers).
    fn schedule_retry(&mut self) {
        if !self.viewer || self.stun.is_none() {
            return;
        }
        let wait = self.retry_after[(self.failures as usize).min(self.retry_after.len() - 1)];
        self.failures += 1;
        let (events, attempt) = (self.events.clone(), self.attempt);
        log::info!("direct connection: trying again in {wait:?}");
        tokio::spawn(async move {
            tokio::time::sleep(wait).await;
            let _ = events.send(Event::Retry(attempt));
        });
    }

    /// Our offer for `attempt` got no answer in time (viewer): a helper from
    /// before direct connections never answers; a newer one that doesn't is
    /// tried again later.
    pub fn no_answer(&mut self, attempt: u32) {
        if attempt != self.attempt || self.peer.is_some() || self.remote.is_some() {
            return;
        }
        if attempt == 0 {
            self.why_not = Some("the remote computer's helper is too old for direct connections");
        } else {
            self.why_not = Some("no answer from the remote computer");
            self.schedule_retry();
        }
    }

    /// A retry is due (viewer): start the next attempt, unless something
    /// newer happened meanwhile.
    pub fn retry(&mut self, attempt: u32) {
        if !self.viewer || attempt != self.attempt || self.remote.is_some() || self.stun.is_none() {
            return;
        }
        self.attempt += 1;
        self.mine = None;
        self.peer = None;
        self.bind();
    }

    /// The connection for `attempt` is up (or not): switch our sending to it.
    pub fn linked(&mut self, attempt: u32, link: Option<Link>) -> Option<SessionMsg> {
        if attempt != self.attempt {
            if let Some(l) = link {
                l.tx.close(); // too late: a newer attempt runs
            }
            return None;
        }
        let Some(link) = link else {
            if self.why_not == Some(LOOKING) {
                self.why_not = Some("no direct path found (firewall or NAT)");
            }
            self.schedule_retry();
            return None;
        };
        log::info!("direct connection to {}", link.remote);
        self.failures = 0;
        self.remote = Some(link.remote);
        self.tx = Some(link.tx);
        self.rx = Some(link.rx);
        self.sending = true;
        Some(SessionMsg::PathSwitch { direct: true })
    }

    /// The other side switched its sending (`PathSwitch` from the relay).
    /// Back to the relay means its connection broke: so did ours.
    pub fn peer_switched(&mut self, direct: bool) -> Option<SessionMsg> {
        if direct {
            self.reading = true;
            None
        } else {
            self.broken()
        }
    }

    /// The direct connection broke: everything goes through the relay again
    /// (the viewer tries again later).
    pub fn broken(&mut self) -> Option<SessionMsg> {
        if let Some(tx) = self.tx.take() {
            tx.close();
        }
        self.rx = None;
        self.reading = false;
        if self.remote.take().is_some() {
            self.why_not = Some("the direct connection broke");
            self.schedule_retry();
        }
        std::mem::take(&mut self.sending).then_some(SessionMsg::PathSwitch { direct: false })
    }

    /// Send `msg` on the direct path if it is in use. Returns what must go
    /// on the relay instead, in order (nothing when sent directly).
    pub async fn send(&mut self, msg: Message) -> Vec<Message> {
        if self.sending
            && let Some(tx) = &mut self.tx
        {
            if tx.send(&msg).await.is_ok() {
                return Vec::new();
            }
            log::info!("direct connection lost, back to the relay");
            let mut out: Vec<Message> = self.broken().iter().map(json).collect();
            out.push(msg);
            return out;
        }
        vec![msg]
    }

    /// Next message on the direct path, once the other side sends there
    /// (never otherwise: for `select!`). An error means it broke: call
    /// [`Self::broken`].
    pub async fn recv(&mut self) -> Result<Message> {
        match &mut self.rx {
            Some(rx) if self.reading => rx.recv().await,
            _ => std::future::pending().await,
        }
    }

    /// The other side's address while the direct path carries anything.
    pub fn remote(&self) -> Option<SocketAddr> {
        self.remote.filter(|_| self.sending || self.reading)
    }

    /// Shorter retry delays (tests).
    #[doc(hidden)]
    pub fn set_retry_after(&mut self, delays: &[Duration]) {
        if !delays.is_empty() {
            self.retry_after = delays.to_vec();
        }
    }
}

/// A session message as a WebSocket text message.
pub fn json(m: &SessionMsg) -> Message {
    Message::Text(serde_json::to_string(m).expect("serializable").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local() -> DirectEndpoint {
        let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = sock.local_addr().unwrap();
        DirectEndpoint::with_socket(sock, vec![addr]).unwrap()
    }

    #[test]
    fn finds_relay_host_and_lan_addresses() {
        assert_eq!(
            relay_host("https://rd.example.com"),
            Some("rd.example.com".into())
        );
        assert_eq!(relay_host("http://10.0.0.5:8470/"), Some("10.0.0.5".into()));
        assert_eq!(
            relay_host("wss://[2001:db8::1]:443/ws"),
            Some("2001:db8::1".into())
        );
        assert_eq!(relay_host("https://"), None);
        assert!(is_lan("192.168.1.4".parse().unwrap()));
        assert!(is_lan("10.1.2.3".parse().unwrap()));
        assert!(is_lan("fd00::1".parse().unwrap()));
        assert!(!is_lan("203.0.113.9".parse().unwrap()));
        assert!(!is_lan("2001:db8::1".parse().unwrap()));
    }

    #[test]
    fn candidates_and_hard_nat_from_what_the_servers_saw() {
        let a = |s: &str| -> SocketAddr { s.parse().unwrap() };
        let lan = Some(a("192.168.1.10:5000"));
        // Relay and public servers agree: easy NAT, LAN + public address.
        assert_eq!(
            candidates(lan, &[a("203.0.113.5:6000"), a("203.0.113.5:6000")]),
            (vec![a("192.168.1.10:5000"), a("203.0.113.5:6000")], false)
        );
        // A relay on the same network sees the LAN address: no hard NAT for that.
        assert_eq!(
            candidates(
                lan,
                &[
                    a("192.168.1.10:5000"),
                    a("203.0.113.5:6000"),
                    a("203.0.113.5:6000")
                ]
            ),
            (vec![a("192.168.1.10:5000"), a("203.0.113.5:6000")], false),
            "the public address the internet servers saw is offered too"
        );
        // Internet servers disagree: hard NAT, LAN only.
        assert_eq!(
            candidates(lan, &[a("203.0.113.5:6000"), a("203.0.113.5:6001")]),
            (vec![a("192.168.1.10:5000")], true)
        );
        // Nobody answered: LAN only, not known to be hard.
        assert_eq!(candidates(lan, &[]), (vec![a("192.168.1.10:5000")], false));
        assert_eq!(candidates(None, &[]), (vec![], false));
    }

    #[tokio::test]
    async fn connects_both_ways_and_carries_messages() {
        let (v, h) = (local(), local());
        let (vfp, hfp, vaddrs, haddrs) = (
            v.fingerprint.clone(),
            h.fingerprint.clone(),
            v.addrs.clone(),
            h.addrs.clone(),
        );
        let viewer = tokio::spawn(async move {
            v.establish(true, &hfp, &haddrs, Duration::from_secs(5))
                .await
        });
        let host = tokio::spawn(async move {
            h.establish(false, &vfp, &vaddrs, Duration::from_secs(5))
                .await
        });
        let (mut v, mut h) = (
            viewer.await.unwrap().expect("viewer side"),
            host.await.unwrap().expect("host side"),
        );
        v.tx.send(&Message::Text(r#"{"t":"refresh"}"#.into()))
            .await
            .unwrap();
        let big: Vec<u8> = (0..3_000_000u32).map(|i| i as u8).collect();
        h.tx.send(&Message::Binary(big.clone().into()))
            .await
            .unwrap();
        assert_eq!(
            h.rx.recv().await.unwrap(),
            Message::Text(r#"{"t":"refresh"}"#.into())
        );
        assert_eq!(v.rx.recv().await.unwrap(), Message::Binary(big.into()));
        v.tx.close();
        assert!(
            h.rx.recv().await.is_err(),
            "closing ends the other side too"
        );
    }

    /// Reading must survive being interrupted: in the session loops the
    /// read races the relay, input and timers, and a frame arrives in many
    /// pieces. Losing a piece once made the link look broken (the session
    /// flapped between direct and relay).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reading_survives_interruptions_mid_frame() {
        let (v, h) = (local(), local());
        let (vfp, hfp, vaddrs, haddrs) = (
            v.fingerprint.clone(),
            h.fingerprint.clone(),
            v.addrs.clone(),
            h.addrs.clone(),
        );
        let viewer = tokio::spawn(async move {
            v.establish(true, &hfp, &haddrs, Duration::from_secs(5))
                .await
        });
        let host = tokio::spawn(async move {
            h.establish(false, &vfp, &vaddrs, Duration::from_secs(5))
                .await
        });
        let (mut v, mut h) = (viewer.await.unwrap().unwrap(), host.await.unwrap().unwrap());
        const N: usize = 40;
        let sender = tokio::spawn(async move {
            for i in 0..N {
                let pic: Vec<u8> = (0..500_000u32).map(|j| (j as usize + i) as u8).collect();
                h.tx.send(&Message::Binary(pic.into())).await.unwrap();
            }
            h
        });
        let mut tick = tokio::time::interval(Duration::from_micros(200));
        let mut got = 0;
        while got < N {
            tokio::select! {
                m = v.rx.recv() => {
                    let Message::Binary(b) = m.expect("frame intact") else { panic!("not a picture") };
                    assert_eq!(b.len(), 500_000);
                    assert!(b.iter().enumerate().all(|(j, &x)| x == (j + got) as u8), "picture {got} intact");
                    got += 1;
                }
                _ = tick.tick() => {} // something else needs attention meanwhile
            }
        }
        let _h = sender.await.unwrap();
    }

    #[tokio::test]
    async fn refuses_a_peer_with_another_certificate() {
        let (v, h, stranger) = (local(), local(), local());
        let (hfp, haddrs) = (h.fingerprint.clone(), h.addrs.clone());
        let vaddrs = v.addrs.clone();
        // The host expects `stranger`, but the viewer connects.
        let host = tokio::spawn(async move {
            h.establish(
                false,
                &stranger.fingerprint,
                &vaddrs,
                Duration::from_secs(2),
            )
            .await
        });
        let viewer = tokio::spawn(async move {
            v.establish(true, &hfp, &haddrs, Duration::from_secs(2))
                .await
        });
        assert!(host.await.unwrap().is_none());
        assert!(viewer.await.unwrap().is_none());
    }

    /// One side of a session, with its background events.
    struct Side {
        st: DirectState,
        ev: tokio::sync::mpsc::UnboundedReceiver<Event>,
    }

    /// Hand `msg` (sent on the relay by one side) to `to`; returns its answer
    /// for the relay, if any.
    fn deliver(to: &mut DirectState, msg: SessionMsg) -> Option<SessionMsg> {
        match msg {
            SessionMsg::DirectOffer {
                fp,
                addrs,
                hard_nat,
                attempt,
            } => to.offer(fp, addrs, hard_nat, attempt),
            SessionMsg::PathSwitch { direct } => to.peer_switched(direct),
            _ => None,
        }
    }

    /// Plays the relay: runs both sides' events and passes their messages
    /// across until `done` holds.
    async fn pump(v: &mut Side, h: &mut Side, done: impl Fn(&DirectState, &DirectState) -> bool) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !done(&v.st, &h.st) {
                let (ev, from_viewer) = tokio::select! {
                    Some(e) = v.ev.recv() => (e, true),
                    Some(e) = h.ev.recv() => (e, false),
                };
                let (me, other) = if from_viewer {
                    (&mut v.st, &mut h.st)
                } else {
                    (&mut h.st, &mut v.st)
                };
                let mut say = match ev {
                    Event::Bound(n, ep) => me.bound(n, ep),
                    Event::Linked(n, l) => me.linked(n, l),
                    Event::Retry(n) => {
                        me.retry(n);
                        None
                    }
                    Event::NoAnswer(n) => {
                        me.no_answer(n);
                        None
                    }
                };
                // Messages bounce until nobody has anything more to say.
                let mut to_other = true;
                while let Some(m) = say {
                    say = if to_other {
                        deliver(other, m)
                    } else {
                        deliver(me, m)
                    };
                    to_other = !to_other;
                }
            }
        })
        .await
        .expect("never got there");
    }

    /// A viewer and a host whose relay allows direct connections (STUN on loopback).
    async fn pair() -> (Side, Side) {
        let stun = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = stun.local_addr().unwrap().port();
        tokio::spawn(crate::stun::serve(stun));
        let (mut v, vev) = DirectState::new(true);
        let (mut h, hev) = DirectState::new(false);
        v.set_retry_after(&[Duration::from_millis(50)]);
        v.allowed("http://127.0.0.1:1", port, vec![]);
        h.allowed("http://127.0.0.1:1", port, vec![]);
        (Side { st: v, ev: vev }, Side { st: h, ev: hev })
    }

    fn both_direct(v: &DirectState, h: &DirectState) -> bool {
        v.remote().is_some() && h.remote().is_some() && v.reading && h.reading
    }

    #[tokio::test]
    async fn both_sides_switch_to_a_direct_path() {
        let (mut v, mut h) = pair().await;
        pump(&mut v, &mut h, both_direct).await;
        assert!(v.st.why_not().is_none());
        let pic = Message::Binary(vec![1, 2, 3].into());
        assert!(
            h.st.send(pic.clone()).await.is_empty(),
            "sent directly, nothing for the relay"
        );
        assert_eq!(v.st.recv().await.unwrap(), pic);
    }

    #[tokio::test]
    async fn a_broken_direct_path_falls_back_and_comes_back() {
        let (mut v, mut h) = pair().await;
        pump(&mut v, &mut h, both_direct).await;
        let pic = Message::Binary(vec![1, 2, 3].into());

        // The host's side notices first (its write failed, or it lost the connection).
        let back = h.st.broken();
        assert_eq!(
            back,
            Some(SessionMsg::PathSwitch { direct: false }),
            "tells the viewer through the relay"
        );
        assert_eq!(h.st.broken(), None, "only once");
        assert_eq!(
            h.st.send(pic.clone()).await,
            vec![pic.clone()],
            "now everything goes on the relay"
        );
        // The viewer's direct path ends at once (the host closed it)...
        assert!(
            tokio::time::timeout(Duration::from_secs(2), v.st.recv())
                .await
                .expect("noticed quickly")
                .is_err()
        );
        // ... and with the host's switch (through the relay) its sending falls back too.
        assert_eq!(
            deliver(&mut v.st, back.unwrap()),
            Some(SessionMsg::PathSwitch { direct: false })
        );
        assert_eq!(v.st.send(pic.clone()).await, vec![pic.clone()]);
        assert_eq!(v.st.why_not(), Some("the direct connection broke"));
        // With nothing to read directly, `recv` waits forever (the relay is read instead).
        assert!(
            tokio::time::timeout(Duration::from_millis(100), v.st.recv())
                .await
                .is_err()
        );

        // The viewer tries again a little later; the host follows its new offer.
        pump(&mut v, &mut h, both_direct).await;
        assert_eq!((v.st.attempt, h.st.attempt), (1, 1));
        assert!(v.st.send(pic.clone()).await.is_empty());
        assert_eq!(
            h.st.recv().await.unwrap(),
            pic,
            "the new connection carries messages"
        );
    }

    #[tokio::test]
    async fn a_failed_write_falls_back_with_the_message() {
        let (mut v, mut h) = pair().await;
        pump(&mut v, &mut h, both_direct).await;
        drop(h); // the other side vanished
        let pic = Message::Binary(vec![9; 100_000].into());
        let mut relay = Vec::new();
        for _ in 0..200 {
            relay = v.st.send(pic.clone()).await;
            if !relay.is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            relay,
            vec![json(&SessionMsg::PathSwitch { direct: false }), pic],
            "switch marker first, then the message, on the relay"
        );
        assert!(v.st.remote().is_none());
    }

    #[tokio::test]
    async fn a_failed_attempt_is_tried_again() {
        let (mut v, _h) = pair().await;
        let Some(Event::Bound(0, ep)) = v.ev.recv().await else {
            panic!()
        };
        assert!(v.st.bound(0, ep).is_some());
        assert_eq!(
            v.st.linked(0, None),
            None,
            "no connection: nothing to switch"
        );
        assert_eq!(
            v.st.why_not(),
            Some("no direct path found (firewall or NAT)")
        );
        let Some(Event::Retry(0)) = v.ev.recv().await else {
            panic!("a retry is due")
        };
        v.st.retry(0);
        let Some(Event::Bound(1, _)) = v.ev.recv().await else {
            panic!("attempt 1 binds a new socket")
        };
        // Late news about attempt 0 changes nothing.
        assert_eq!(v.st.linked(0, None), None);
        assert_eq!(v.st.attempt, 1);
        v.st.retry(0);
        assert_eq!(v.st.attempt, 1, "a stale retry starts nothing");
    }

    #[tokio::test]
    async fn an_old_helper_that_never_answers_is_named() {
        let (mut v, _h) = pair().await; // the host's offers never reach the viewer
        let Some(Event::Bound(0, ep)) = v.ev.recv().await else {
            panic!()
        };
        assert!(v.st.bound(0, ep).is_some());
        assert_eq!(v.st.why_not(), Some(LOOKING));
        v.st.no_answer(0);
        assert_eq!(
            v.st.why_not(),
            Some("the remote computer's helper is too old for direct connections")
        );
        assert_eq!(v.st.attempt, 0, "no point trying again");
        // An answer that did come makes it a non-event.
        let (mut w, mut hh) = pair().await;
        pump(&mut w, &mut hh, both_direct).await;
        w.st.no_answer(0);
        assert!(w.st.why_not().is_none());
    }

    #[tokio::test]
    async fn only_the_viewer_starts_new_attempts() {
        let (mut v, mut h) = pair().await;
        let offer = |attempt| {
            (
                "ab".to_string(),
                vec!["127.0.0.1:9".parse().unwrap()],
                false,
                attempt,
            )
        };
        let (fp, addrs, hard, _) = offer(5);
        assert_eq!(v.st.offer(fp, addrs, hard, 5), None);
        assert_eq!(v.st.attempt, 0, "a host can't make the viewer start over");
        let (fp, addrs, hard, _) = offer(3);
        h.st.offer(fp, addrs, hard, 3);
        assert_eq!(h.st.attempt, 3, "the host follows the viewer");
        h.st.retry(3);
        assert_eq!(h.st.attempt, 3, "the host never retries on its own");
        // It binds a socket for attempt 3 (after the one for attempt 0).
        loop {
            match h.ev.recv().await {
                Some(Event::Bound(3, _)) => break,
                Some(Event::Bound(0, _)) => {}
                _ => panic!("expected a socket for attempt 3"),
            }
        }
    }

    #[tokio::test]
    async fn the_other_side_going_back_to_the_relay_ends_ours_too() {
        let (mut v, mut h) = pair().await;
        pump(&mut v, &mut h, both_direct).await;
        assert_eq!(
            v.st.peer_switched(false),
            Some(SessionMsg::PathSwitch { direct: false })
        );
        let m = Message::Text("x".into());
        assert_eq!(v.st.send(m.clone()).await, vec![m]);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), h.st.recv())
                .await
                .unwrap()
                .is_err()
        );
    }

    #[tokio::test]
    async fn says_why_there_is_no_direct_path() {
        let (mut v, _e) = DirectState::new(true);
        assert_eq!(v.why_not(), Some("the server allows no direct connections"));
        assert_eq!(
            v.offer(
                "ab".into(),
                vec!["203.0.113.1:5".parse().unwrap()],
                false,
                0
            ),
            None
        );
        assert!(
            v.peer.is_none(),
            "offers are ignored unless the relay allowed direct connections"
        );
        let (mut v, _w) = pair().await;
        v.st.offer("ab".into(), vec!["203.0.113.1:5".parse().unwrap()], true, 0);
        assert_eq!(
            v.st.why_not(),
            Some("the remote computer is behind a strict NAT")
        );
        assert_eq!(v.st.bound(0, Err(anyhow!("udp blocked"))), None);
        assert_eq!(v.st.why_not(), Some("no UDP connection to the server"));
    }

    #[tokio::test]
    async fn gives_up_when_nobody_answers() {
        let v = local();
        let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let addr = silent.local_addr().unwrap();
        let t = std::time::Instant::now();
        assert!(
            v.establish(true, "00", &[addr], Duration::from_millis(500))
                .await
                .is_none()
        );
        assert!(t.elapsed() < Duration::from_secs(2));
    }
}
