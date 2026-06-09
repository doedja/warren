//! Minimal SOCKS5 server (RFC 1928 + 1929 username/password auth).
//!
//! CONNECT (TCP) and UDP ASSOCIATE (UDP relay) are supported. [`negotiate`] runs
//! the method handshake, optional user/pass auth, and reads the request,
//! returning a [`Socks5Req`]. For CONNECT the caller dials a node and sends
//! [`write_reply`] before splicing; for UDP ASSOCIATE the caller binds a relay
//! socket and replies with its address via [`write_reply_addr`].

use std::net::SocketAddr;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const VER: u8 = 0x05;
const METHOD_NOAUTH: u8 = 0x00;
const METHOD_USERPASS: u8 = 0x02;
const METHOD_NONE: u8 = 0xFF;
const CMD_CONNECT: u8 = 0x01;
const CMD_UDP_ASSOCIATE: u8 = 0x03;
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_IPV6: u8 = 0x04;

/// SOCKS5 reply codes (subset).
pub const REP_SUCCESS: u8 = 0x00;
pub const REP_GENERAL_FAILURE: u8 = 0x01;
const REP_CMD_NOT_SUPPORTED: u8 = 0x07;
const REP_ATYP_NOT_SUPPORTED: u8 = 0x08;

/// What the client asked for after the handshake.
#[derive(Debug, PartialEq)]
pub enum Socks5Req {
    /// CONNECT to a TCP target.
    Connect { host: String, port: u16 },
    /// UDP ASSOCIATE: the client wants a UDP relay. Its declared source address
    /// is advisory (often 0.0.0.0:0), so we drop it and learn the real source
    /// from the first datagram.
    UdpAssociate,
}

/// Username/password verifier: `(user, pass) -> ok`.
pub type Verifier = dyn Fn(&str, &str) -> bool + Send + Sync;

fn invalid(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
}

/// Run the SOCKS5 handshake. Returns `Some(Socks5Req)` when a CONNECT or UDP
/// ASSOCIATE request is accepted; `None` when the client was rejected (auth /
/// method failure or unsupported command), with the rejection already written.
pub async fn negotiate<S: AsyncRead + AsyncWrite + Unpin>(
    s: &mut S,
    verify: Option<&Verifier>,
) -> std::io::Result<Option<Socks5Req>> {
    // Greeting: VER, NMETHODS, METHODS...
    let mut head = [0u8; 2];
    s.read_exact(&mut head).await?;
    if head[0] != VER {
        return Err(invalid("not SOCKS5"));
    }
    let nmethods = head[1] as usize;
    let mut methods = vec![0u8; nmethods];
    s.read_exact(&mut methods).await?;

    // Method selection.
    match verify {
        Some(v) => {
            if !methods.contains(&METHOD_USERPASS) {
                s.write_all(&[VER, METHOD_NONE]).await?;
                s.flush().await?;
                return Ok(None);
            }
            s.write_all(&[VER, METHOD_USERPASS]).await?;
            s.flush().await?;
            if !userpass_auth(s, v).await? {
                return Ok(None);
            }
        }
        None => {
            if !methods.contains(&METHOD_NOAUTH) {
                s.write_all(&[VER, METHOD_NONE]).await?;
                s.flush().await?;
                return Ok(None);
            }
            s.write_all(&[VER, METHOD_NOAUTH]).await?;
            s.flush().await?;
        }
    }

    // Request: VER, CMD, RSV, ATYP, ADDR, PORT
    let mut req = [0u8; 4];
    s.read_exact(&mut req).await?;
    if req[0] != VER {
        return Err(invalid("bad request version"));
    }
    // RFC 1928: an unsupported ATYP gets reply 0x08, not a raw close (the
    // client could not otherwise tell a proxy error from a network failure).
    if !matches!(req[3], ATYP_IPV4 | ATYP_DOMAIN | ATYP_IPV6) {
        write_reply(s, REP_ATYP_NOT_SUPPORTED).await?;
        return Ok(None);
    }
    match req[1] {
        CMD_CONNECT => {
            let (host, port) = read_addr_port(s, req[3]).await?;
            // A zero-length domain parses but can never dial; reject here
            // instead of burning a node dial on it.
            if host.is_empty() {
                write_reply(s, REP_GENERAL_FAILURE).await?;
                return Ok(None);
            }
            Ok(Some(Socks5Req::Connect { host, port }))
        }
        CMD_UDP_ASSOCIATE => {
            // Consume the declared source addr+port (advisory; we learn the real
            // source from the first relayed datagram) and accept the associate.
            let _ = read_addr_port(s, req[3]).await?;
            Ok(Some(Socks5Req::UdpAssociate))
        }
        _ => {
            write_reply(s, REP_CMD_NOT_SUPPORTED).await?;
            Ok(None)
        }
    }
}

/// Read ATYP-tagged address + 2-byte port from the request body.
async fn read_addr_port<S: AsyncRead + Unpin>(
    s: &mut S,
    atyp: u8,
) -> std::io::Result<(String, u16)> {
    let host = match atyp {
        ATYP_IPV4 => {
            let mut a = [0u8; 4];
            s.read_exact(&mut a).await?;
            std::net::Ipv4Addr::new(a[0], a[1], a[2], a[3]).to_string()
        }
        ATYP_DOMAIN => {
            let mut len = [0u8; 1];
            s.read_exact(&mut len).await?;
            let mut name = vec![0u8; len[0] as usize];
            s.read_exact(&mut name).await?;
            String::from_utf8(name).map_err(|_| invalid("bad domain"))?
        }
        ATYP_IPV6 => {
            let mut a = [0u8; 16];
            s.read_exact(&mut a).await?;
            let segs: [u16; 8] =
                std::array::from_fn(|i| u16::from_be_bytes([a[2 * i], a[2 * i + 1]]));
            std::net::Ipv6Addr::new(
                segs[0], segs[1], segs[2], segs[3], segs[4], segs[5], segs[6], segs[7],
            )
            .to_string()
        }
        _ => return Err(invalid("address type not supported")),
    };
    let mut port = [0u8; 2];
    s.read_exact(&mut port).await?;
    Ok((host, u16::from_be_bytes(port)))
}

async fn userpass_auth<S: AsyncRead + AsyncWrite + Unpin>(
    s: &mut S,
    verify: &Verifier,
) -> std::io::Result<bool> {
    // VER(=1), ULEN, UNAME, PLEN, PASSWD
    let mut ver = [0u8; 1];
    s.read_exact(&mut ver).await?;
    if ver[0] != 0x01 {
        return Err(invalid("bad auth version"));
    }
    let mut ulen = [0u8; 1];
    s.read_exact(&mut ulen).await?;
    let mut user = vec![0u8; ulen[0] as usize];
    s.read_exact(&mut user).await?;
    let mut plen = [0u8; 1];
    s.read_exact(&mut plen).await?;
    let mut pass = vec![0u8; plen[0] as usize];
    s.read_exact(&mut pass).await?;

    let ok = verify(
        &String::from_utf8_lossy(&user),
        &String::from_utf8_lossy(&pass),
    );
    s.write_all(&[0x01, if ok { 0x00 } else { 0x01 }]).await?;
    s.flush().await?;
    Ok(ok)
}

/// Write a SOCKS5 reply with the given code and a zeroed bound address.
pub async fn write_reply<S: AsyncWrite + Unpin>(s: &mut S, code: u8) -> std::io::Result<()> {
    // VER, REP, RSV, ATYP=IPv4, BND.ADDR=0.0.0.0, BND.PORT=0
    s.write_all(&[VER, code, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await?;
    s.flush().await
}

/// Write a SOCKS5 reply carrying a real bound address. Used for UDP ASSOCIATE,
/// where the client must learn the relay socket to send its datagrams to.
pub async fn write_reply_addr<S: AsyncWrite + Unpin>(
    s: &mut S,
    code: u8,
    addr: SocketAddr,
) -> std::io::Result<()> {
    let mut out = vec![VER, code, 0x00];
    match addr {
        SocketAddr::V4(a) => {
            out.push(ATYP_IPV4);
            out.extend_from_slice(&a.ip().octets());
        }
        SocketAddr::V6(a) => {
            out.push(ATYP_IPV6);
            out.extend_from_slice(&a.ip().octets());
        }
    }
    out.extend_from_slice(&addr.port().to_be_bytes());
    s.write_all(&out).await?;
    s.flush().await
}

/// Parse a SOCKS5 UDP request header (RFC 1928 sec 7) off a datagram the client
/// sent to the relay: RSV(2) FRAG(1) ATYP ADDR PORT(2) DATA. Returns
/// `(host, port, data_offset)`. Returns None on a short buffer, an unknown ATYP,
/// or a fragmented datagram (FRAG != 0), which we do not reassemble.
pub fn parse_udp_header(buf: &[u8]) -> Option<(String, u16, usize)> {
    if buf.len() < 4 || buf[2] != 0 {
        return None;
    }
    let (host, i) = match buf[3] {
        ATYP_IPV4 => {
            if buf.len() < 10 {
                return None;
            }
            (
                std::net::Ipv4Addr::new(buf[4], buf[5], buf[6], buf[7]).to_string(),
                8,
            )
        }
        ATYP_DOMAIN => {
            let len = *buf.get(4)? as usize;
            let end = 5 + len;
            if buf.len() < end + 2 {
                return None;
            }
            (String::from_utf8(buf[5..end].to_vec()).ok()?, end)
        }
        ATYP_IPV6 => {
            if buf.len() < 22 {
                return None;
            }
            let segs: [u16; 8] =
                std::array::from_fn(|k| u16::from_be_bytes([buf[4 + 2 * k], buf[4 + 2 * k + 1]]));
            (
                std::net::Ipv6Addr::new(
                    segs[0], segs[1], segs[2], segs[3], segs[4], segs[5], segs[6], segs[7],
                )
                .to_string(),
                20,
            )
        }
        _ => return None,
    };
    let port = u16::from_be_bytes([buf[i], buf[i + 1]]);
    Some((host, port, i + 2))
}

/// Build a SOCKS5 UDP reply datagram for a packet coming back from `host:port`:
/// the RSV/FRAG/ATYP/ADDR/PORT header followed by `data`, ready to send to the
/// client's UDP socket.
pub fn wrap_udp(host: &str, port: u16, data: &[u8]) -> Option<Vec<u8>> {
    let mut out = vec![0x00, 0x00, 0x00]; // RSV, RSV, FRAG=0
    if let Ok(v4) = host.parse::<std::net::Ipv4Addr>() {
        out.push(ATYP_IPV4);
        out.extend_from_slice(&v4.octets());
    } else if let Ok(v6) = host.parse::<std::net::Ipv6Addr>() {
        out.push(ATYP_IPV6);
        out.extend_from_slice(&v6.octets());
    } else {
        let h = host.as_bytes();
        // SOCKS5 domain length is a single byte; a longer name cannot be encoded
        // without silent truncation, so drop the datagram instead.
        if h.len() > 255 {
            return None;
        }
        out.push(ATYP_DOMAIN);
        out.push(h.len() as u8);
        out.extend_from_slice(h);
    }
    out.extend_from_slice(&port.to_be_bytes());
    out.extend_from_slice(data);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a no-auth SOCKS5 CONNECT to a domain target, return the bytes.
    fn connect_domain(host: &str, port: u16) -> Vec<u8> {
        let mut v = vec![VER, 1, METHOD_NOAUTH]; // greeting: 1 method, noauth
        v.extend_from_slice(&[VER, CMD_CONNECT, 0x00, 0x03]);
        v.push(host.len() as u8);
        v.extend_from_slice(host.as_bytes());
        v.extend_from_slice(&port.to_be_bytes());
        v
    }

    #[tokio::test]
    async fn negotiate_noauth_domain() {
        let bytes = connect_domain("example.com", 443);
        let server = bytes; // client-to-server stream
        let mut io = tokio_test_duplex(server).await;
        let got = negotiate(&mut io, None).await.unwrap();
        assert_eq!(
            got,
            Some(Socks5Req::Connect {
                host: "example.com".to_string(),
                port: 443
            })
        );
    }

    #[tokio::test]
    async fn negotiate_udp_associate() {
        // greeting + request: CMD=UDP_ASSOCIATE, ATYP=IPv4, 0.0.0.0:0 (advisory).
        let mut v = vec![VER, 1, METHOD_NOAUTH];
        v.extend_from_slice(&[VER, CMD_UDP_ASSOCIATE, 0x00, ATYP_IPV4, 0, 0, 0, 0, 0, 0]);
        let mut io = tokio_test_duplex(v).await;
        let got = negotiate(&mut io, None).await.unwrap();
        assert_eq!(got, Some(Socks5Req::UdpAssociate));
    }

    #[test]
    fn udp_header_roundtrip() {
        // wrap_udp builds a reply header; parse_udp_header reads a request header.
        // Same on-wire layout, so a wrap then parse round-trips host/port/data.
        let dgram = wrap_udp("1.1.1.1", 53, b"\xde\xad").expect("wrap");
        let (host, port, off) = parse_udp_header(&dgram).expect("parse");
        assert_eq!((host.as_str(), port), ("1.1.1.1", 53));
        assert_eq!(&dgram[off..], b"\xde\xad");
        // A domain longer than 255 bytes cannot be SOCKS5-encoded: drop it.
        assert!(wrap_udp(&"a".repeat(256), 53, b"x").is_none());
    }

    #[test]
    fn udp_header_domain_and_fragment() {
        let dgram = wrap_udp("dns.example", 5353, b"x").expect("wrap");
        let (host, port, off) = parse_udp_header(&dgram).expect("parse domain");
        assert_eq!((host.as_str(), port), ("dns.example", 5353));
        assert_eq!(&dgram[off..], b"x");
        // FRAG != 0 is rejected (we do not reassemble).
        let mut frag = dgram.clone();
        frag[2] = 1;
        assert!(parse_udp_header(&frag).is_none());
    }

    #[tokio::test]
    async fn negotiate_rejects_when_auth_required_but_not_offered() {
        let bytes = connect_domain("h", 80); // offers only noauth
        let mut io = tokio_test_duplex(bytes).await;
        let verify = |u: &str, p: &str| u == "u" && p == "p";
        let got = negotiate(&mut io, Some(&verify)).await.unwrap();
        assert_eq!(got, None);
    }

    /// Wrap a byte vec as a readable/writable in-memory stream (reads return the
    /// vec; writes are discarded into a buffer we ignore).
    async fn tokio_test_duplex(client_bytes: Vec<u8>) -> tokio::io::DuplexStream {
        let (mut feeder, server) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let _ = feeder.write_all(&client_bytes).await;
            // keep feeder open a moment so server can also write replies
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        });
        server
    }
}
