//! Minimal SOCKS5 server (RFC 1928 + 1929 username/password auth).
//!
//! Only the CONNECT command is supported (TCP). [`negotiate`] runs the method
//! handshake, optional user/pass auth, and reads the CONNECT request, returning
//! the target. The caller dials a node, then sends [`write_reply`] before
//! splicing.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const VER: u8 = 0x05;
const METHOD_NOAUTH: u8 = 0x00;
const METHOD_USERPASS: u8 = 0x02;
const METHOD_NONE: u8 = 0xFF;
const CMD_CONNECT: u8 = 0x01;

/// SOCKS5 reply codes (subset).
pub const REP_SUCCESS: u8 = 0x00;
pub const REP_GENERAL_FAILURE: u8 = 0x01;

/// Username/password verifier: `(user, pass) -> ok`.
pub type Verifier = dyn Fn(&str, &str) -> bool + Send + Sync;

fn invalid(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
}

/// Run the SOCKS5 handshake. Returns `Some((host, port))` when a CONNECT
/// request is accepted and ready to dial; `None` when the client was rejected
/// (auth/method failure or unsupported command), with the rejection already
/// written.
pub async fn negotiate<S: AsyncRead + AsyncWrite + Unpin>(
    s: &mut S,
    verify: Option<&Verifier>,
) -> std::io::Result<Option<(String, u16)>> {
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
    if req[1] != CMD_CONNECT {
        write_reply(s, 0x07).await?; // command not supported
        return Ok(None);
    }
    let host = match req[3] {
        0x01 => {
            let mut a = [0u8; 4];
            s.read_exact(&mut a).await?;
            std::net::Ipv4Addr::new(a[0], a[1], a[2], a[3]).to_string()
        }
        0x03 => {
            let mut len = [0u8; 1];
            s.read_exact(&mut len).await?;
            let mut name = vec![0u8; len[0] as usize];
            s.read_exact(&mut name).await?;
            String::from_utf8(name).map_err(|_| invalid("bad domain"))?
        }
        0x04 => {
            let mut a = [0u8; 16];
            s.read_exact(&mut a).await?;
            let segs: [u16; 8] =
                std::array::from_fn(|i| u16::from_be_bytes([a[2 * i], a[2 * i + 1]]));
            std::net::Ipv6Addr::new(
                segs[0], segs[1], segs[2], segs[3], segs[4], segs[5], segs[6], segs[7],
            )
            .to_string()
        }
        _ => {
            write_reply(s, 0x08).await?; // address type not supported
            return Ok(None);
        }
    };
    let mut port = [0u8; 2];
    s.read_exact(&mut port).await?;
    let port = u16::from_be_bytes(port);

    Ok(Some((host, port)))
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
        assert_eq!(got, Some(("example.com".to_string(), 443)));
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
