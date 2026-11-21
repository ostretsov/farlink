//! Minimal STUN (RFC 5389) Binding: "what address does the internet see me
//! as?". The relay answers on UDP ([`serve`]); viewer and helper ask it
//! ([`query`]) from the very socket their direct QUIC connection then uses
//! (see [`crate::direct`]), so the answer is the address the other side must
//! send to.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::Duration;

const MAGIC: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const XOR_MAPPED_ADDRESS: u16 = 0x0020;
const HEADER: usize = 20;

/// Default UDP port of the relay's STUN answerer.
pub const DEFAULT_PORT: u16 = 3478;

pub type TxId = [u8; 12];

fn header(kind: u16, len: u16, tx: &TxId) -> Vec<u8> {
    let mut b = Vec::with_capacity(HEADER + len as usize);
    b.extend_from_slice(&kind.to_be_bytes());
    b.extend_from_slice(&len.to_be_bytes());
    b.extend_from_slice(&MAGIC.to_be_bytes());
    b.extend_from_slice(tx);
    b
}

/// A Binding request with transaction id `tx`.
pub fn request(tx: &TxId) -> Vec<u8> {
    header(BINDING_REQUEST, 0, tx)
}

/// `(kind, transaction id, attributes)` of a STUN message, if `buf` is one.
fn parse(buf: &[u8]) -> Option<(u16, TxId, &[u8])> {
    if buf.len() < HEADER || buf[0] & 0xC0 != 0 {
        return None;
    }
    let kind = u16::from_be_bytes([buf[0], buf[1]]);
    let len = u16::from_be_bytes([buf[2], buf[3]]) as usize;
    if u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]) != MAGIC || buf.len() < HEADER + len {
        return None;
    }
    let tx: TxId = buf[8..20].try_into().ok()?;
    Some((kind, tx, &buf[HEADER..HEADER + len]))
}

/// The transaction id of a Binding request.
pub fn parse_request(buf: &[u8]) -> Option<TxId> {
    parse(buf).and_then(|(kind, tx, _)| (kind == BINDING_REQUEST).then_some(tx))
}

/// The success answer telling the asker it was seen as `addr`.
pub fn response(tx: &TxId, addr: SocketAddr) -> Vec<u8> {
    let port = addr.port() ^ (MAGIC >> 16) as u16;
    let (family, ip): (u8, Vec<u8>) = match addr.ip() {
        IpAddr::V4(v4) => (1, (u32::from(v4) ^ MAGIC).to_be_bytes().to_vec()),
        IpAddr::V6(v6) => (2, v6.octets().iter().zip(xor_key(tx)).map(|(a, k)| a ^ k).collect()),
    };
    let value_len = 4 + ip.len() as u16;
    let mut b = header(BINDING_SUCCESS, 4 + value_len, tx);
    b.extend_from_slice(&XOR_MAPPED_ADDRESS.to_be_bytes());
    b.extend_from_slice(&value_len.to_be_bytes());
    b.extend_from_slice(&[0, family]);
    b.extend_from_slice(&port.to_be_bytes());
    b.extend_from_slice(&ip);
    b
}

fn xor_key(tx: &TxId) -> [u8; 16] {
    let mut k = [0u8; 16];
    k[..4].copy_from_slice(&MAGIC.to_be_bytes());
    k[4..].copy_from_slice(tx);
    k
}

/// The address in a success answer to transaction `tx`.
pub fn parse_response(buf: &[u8], tx: &TxId) -> Option<SocketAddr> {
    let (kind, got, mut attrs) = parse(buf)?;
    if kind != BINDING_SUCCESS || &got != tx {
        return None;
    }
    while attrs.len() >= 4 {
        let t = u16::from_be_bytes([attrs[0], attrs[1]]);
        let len = u16::from_be_bytes([attrs[2], attrs[3]]) as usize;
        let value = attrs.get(4..4 + len)?;
        if t == XOR_MAPPED_ADDRESS && len >= 8 {
            let port = u16::from_be_bytes([value[2], value[3]]) ^ (MAGIC >> 16) as u16;
            let ip = match (value[1], len) {
                (1, 8) => IpAddr::V4(Ipv4Addr::from(u32::from_be_bytes(value[4..8].try_into().ok()?) ^ MAGIC)),
                (2, 20) => {
                    let mut o = [0u8; 16];
                    for (i, (a, k)) in value[4..20].iter().zip(xor_key(tx)).enumerate() {
                        o[i] = a ^ k;
                    }
                    IpAddr::V6(Ipv6Addr::from(o))
                }
                _ => return None,
            };
            return Some(SocketAddr::new(ip, port));
        }
        attrs = &attrs[(4 + len + 3) & !3..]; // attributes are padded to 4 bytes
    }
    None
}

/// Ask `server` how it sees `sock` (three tries, `timeout` in all).
pub async fn query(sock: &tokio::net::UdpSocket, server: SocketAddr, timeout: Duration) -> Option<SocketAddr> {
    query_many(sock, &[server], timeout).await.pop().flatten()
}

/// Ask every server at once from the same socket (three tries each,
/// `timeout` in all); the answers in the order of `servers`.
pub async fn query_many(sock: &tokio::net::UdpSocket, servers: &[SocketAddr], timeout: Duration) -> Vec<Option<SocketAddr>> {
    let txs: Vec<TxId> = servers
        .iter()
        .map(|_| {
            let mut tx: TxId = [0; 12];
            rand::fill(&mut tx[..]);
            tx
        })
        .collect();
    let mut seen: Vec<Option<SocketAddr>> = vec![None; servers.len()];
    let mut buf = [0u8; 512];
    for _ in 0..3 {
        for (i, server) in servers.iter().enumerate() {
            if seen[i].is_none() {
                let _ = sock.send_to(&request(&txs[i]), server).await;
            }
        }
        let _ = tokio::time::timeout(timeout / 3, async {
            while seen.iter().any(Option::is_none) {
                let Ok((n, from)) = sock.recv_from(&mut buf).await else { return };
                for (i, server) in servers.iter().enumerate() {
                    if from == *server
                        && seen[i].is_none()
                        && let Some(addr) = parse_response(&buf[..n], &txs[i])
                    {
                        seen[i] = Some(addr);
                    }
                }
            }
        })
        .await;
        if seen.iter().all(Option::is_some) {
            break;
        }
    }
    seen
}

/// Did different servers see different public addresses for the same
/// socket? Then the NAT in front of it gives every destination its own port
/// ("hard" / symmetric NAT): the address one server saw is useless to a
/// peer, and hole punching won't work.
pub fn hard_nat(seen: &[SocketAddr]) -> bool {
    seen.windows(2).any(|w| w[0] != w[1])
}

/// Answer Binding requests on `sock` forever (the relay).
pub async fn serve(sock: tokio::net::UdpSocket) {
    let mut buf = [0u8; 512];
    loop {
        let Ok((n, from)) = sock.recv_from(&mut buf).await else { continue };
        if let Some(tx) = parse_request(&buf[..n]) {
            let _ = sock.send_to(&response(&tx, from), from).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_round_trip_v4_and_v6() {
        let tx: TxId = *b"abcdefghijkl";
        assert_eq!(parse_request(&request(&tx)), Some(tx));
        for addr in ["203.0.113.7:51234", "[2001:db8::42]:9", "0.0.0.0:0"] {
            let addr: SocketAddr = addr.parse().unwrap();
            assert_eq!(parse_response(&response(&tx, addr), &tx), Some(addr), "{addr}");
        }
    }

    #[test]
    fn rejects_other_messages() {
        let tx: TxId = [1; 12];
        let addr: SocketAddr = "198.51.100.1:7".parse().unwrap();
        assert_eq!(parse_response(&response(&tx, addr), &[2; 12]), None, "other transaction");
        assert_eq!(parse_request(&response(&tx, addr)), None, "an answer is no request");
        assert_eq!(parse_request(b"hello, this is no STUN message"), None);
        assert_eq!(parse_request(&[0u8; 5]), None);
        let mut cut = response(&tx, addr);
        cut.truncate(26);
        assert_eq!(parse_response(&cut, &tx), None);
    }

    #[test]
    fn xor_address_differs_on_the_wire() {
        // NATs that rewrite addresses they find in packets can't spoil it.
        let tx: TxId = [7; 12];
        let addr: SocketAddr = "192.168.1.20:40000".parse().unwrap();
        let wire = response(&tx, addr);
        assert!(!wire.windows(4).any(|w| w == [192, 168, 1, 20]));
    }

    #[test]
    fn hard_nat_when_servers_disagree() {
        let a: SocketAddr = "203.0.113.5:40000".parse().unwrap();
        let b: SocketAddr = "203.0.113.5:40001".parse().unwrap();
        assert!(!hard_nat(&[]));
        assert!(!hard_nat(&[a]));
        assert!(!hard_nat(&[a, a, a]));
        assert!(hard_nat(&[a, a, b]), "a new port for another destination");
    }

    #[tokio::test]
    async fn asks_several_servers_at_once() {
        let mut servers = Vec::new();
        for _ in 0..2 {
            let s = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            servers.push(s.local_addr().unwrap());
            tokio::spawn(serve(s));
        }
        let silent = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        servers.push(silent.local_addr().unwrap());
        let me = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mine = me.local_addr().unwrap();
        let seen = query_many(&me, &servers, Duration::from_millis(600)).await;
        assert_eq!(seen, vec![Some(mine), Some(mine), None]);
    }

    #[tokio::test]
    async fn query_against_served_socket() {
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server_addr = server.local_addr().unwrap();
        tokio::spawn(serve(server));
        let me = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let seen = query(&me, server_addr, Duration::from_secs(2)).await;
        assert_eq!(seen, Some(me.local_addr().unwrap()));
    }
}
