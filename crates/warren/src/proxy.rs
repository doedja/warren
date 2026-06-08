//! Client-facing HTTP CONNECT proxy: request parsing + Basic proxy auth.
//!
//! Hand-rolled (no HTTP library): the hub only needs to read a single CONNECT
//! request line plus headers, then splice raw bytes.

use base64::Engine;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const MAX_HEAD: usize = 8192;

fn invalid(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
}

/// A parsed HTTP request head (request line + headers). One parser serves both
/// CONNECT (tunnel) and plain-HTTP (absolute-URI) requests; the caller branches
/// on `method`.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub target: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
}

impl HttpRequest {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
    pub fn authorization(&self) -> Option<&str> {
        self.header("proxy-authorization")
    }
}

/// Read and parse an HTTP request head (up to the blank line, capped).
pub async fn read_request<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<HttpRequest> {
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        r.read_exact(&mut byte).await?;
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > MAX_HEAD {
            return Err(invalid("request too large"));
        }
    }
    let head = String::from_utf8_lossy(&buf);
    let mut lines = head.split("\r\n");
    let request_line = lines.next().ok_or_else(|| invalid("empty request"))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| invalid("no method"))?
        .to_string();
    let target = parts
        .next()
        .ok_or_else(|| invalid("no target"))?
        .to_string();
    let version = parts.next().unwrap_or("HTTP/1.1").to_string();
    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    Ok(HttpRequest {
        method,
        target,
        version,
        headers,
    })
}

/// From an absolute-URI request, derive (host, port, origin-form path).
fn parse_http_target(target: &str, host_header: Option<&str>) -> Option<(String, u16, String)> {
    let after = target
        .strip_prefix("http://")
        .or_else(|| target.strip_prefix("HTTP://"))?;
    let (authority, path) = match after.find('/') {
        Some(i) => (&after[..i], &after[i..]),
        None => (after, "/"),
    };
    // authority may be user@host:port; drop any userinfo.
    let hostport = authority.rsplit('@').next().unwrap_or(authority);
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().unwrap_or(80)),
        None => (hostport.to_string(), 80),
    };
    let host = if host.is_empty() {
        host_header?.split(':').next().unwrap_or("").to_string()
    } else {
        host
    };
    if host.is_empty() {
        return None;
    }
    Some((host, port, path.to_string()))
}

/// Build the origin-form request to forward to the target, stripping
/// proxy-specific headers and forcing `Connection: close`.
pub fn forward_request(req: &HttpRequest) -> Option<(String, u16, Vec<u8>)> {
    let (host, port, path) = parse_http_target(&req.target, req.header("host"))?;
    let mut out = format!("{} {} {}\r\n", req.method, path, req.version);
    for (k, v) in &req.headers {
        let lk = k.to_ascii_lowercase();
        if lk == "proxy-authorization" || lk == "proxy-connection" || lk == "connection" {
            continue;
        }
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("Connection: close\r\n\r\n");
    Some((host, port, out.into_bytes()))
}

/// Split an HTTP CONNECT target into (host, port). Handles bracketed IPv6
/// literals (`[::1]:443`, `[::1]`), `host:port`, and a bare `host` (default
/// port 443, per the CONNECT convention). Returns None only when an explicit
/// port is present but unparseable.
pub fn split_connect_target(target: &str) -> Option<(String, u16)> {
    if let Some(rest) = target.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None if after.is_empty() => 443,
            None => return None,
        };
        return Some((host.to_string(), port));
    }
    match target.rsplit_once(':') {
        Some((h, p)) => Some((h.to_string(), p.parse().ok()?)),
        None => Some((target.to_string(), 443)),
    }
}

/// Parse a `Basic <base64(user:pass)>` header value into (user, pass).
pub fn parse_basic(authorization: &str) -> Option<(String, String)> {
    let rest = match authorization.get(..6) {
        Some(scheme) if scheme.eq_ignore_ascii_case("basic ") => &authorization[6..],
        _ => return None,
    };
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(rest.trim())
        .ok()?;
    let s = String::from_utf8(decoded).ok()?;
    let (u, p) = s.split_once(':')?;
    Some((u.to_string(), p.to_string()))
}

pub async fn write_established<W: AsyncWrite + Unpin>(w: &mut W) -> std::io::Result<()> {
    w.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    w.flush().await
}

pub async fn write_auth_required<W: AsyncWrite + Unpin>(w: &mut W) -> std::io::Result<()> {
    w.write_all(
        b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"warren\"\r\nContent-Length: 0\r\n\r\n",
    )
    .await?;
    w.flush().await
}

pub async fn write_bad_gateway<W: AsyncWrite + Unpin>(w: &mut W) -> std::io::Result<()> {
    w.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
        .await?;
    w.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn parse_basic_decodes_credentials() {
        let creds = base64::engine::general_purpose::STANDARD.encode("user:pass");
        assert_eq!(
            parse_basic(&format!("Basic {creds}")),
            Some(("user".to_string(), "pass".to_string()))
        );
        assert_eq!(parse_basic("Bearer x"), None);
    }

    #[test]
    fn connect_target_parsing() {
        let s = |h: &str, p: u16| Some((h.to_string(), p));
        assert_eq!(
            split_connect_target("example.com:443"),
            s("example.com", 443)
        );
        assert_eq!(split_connect_target("example.com"), s("example.com", 443)); // default port
        assert_eq!(split_connect_target("[::1]:8443"), s("::1", 8443)); // bracketed IPv6
        assert_eq!(split_connect_target("[2001:db8::1]"), s("2001:db8::1", 443)); // IPv6, default port
        assert_eq!(split_connect_target("example.com:notaport"), None); // bad explicit port
        assert_eq!(split_connect_target("[::1]:bad"), None); // bad bracketed port
    }

    #[tokio::test]
    async fn plain_http_rewrites_to_origin_form() {
        let raw = "GET http://example.com:8080/a/b?c=1 HTTP/1.1\r\nHost: example.com:8080\r\nProxy-Authorization: Basic x\r\nUser-Agent: t\r\n\r\n";
        let mut r = std::io::Cursor::new(raw.as_bytes().to_vec());
        let req = read_request(&mut r).await.unwrap();
        assert_eq!(req.method, "GET");
        let (host, port, head) = forward_request(&req).unwrap();
        assert_eq!(host, "example.com");
        assert_eq!(port, 8080);
        let head_s = String::from_utf8(head).unwrap();
        assert!(head_s.starts_with("GET /a/b?c=1 HTTP/1.1\r\n"));
        assert!(!head_s.to_lowercase().contains("proxy-authorization"));
        assert!(head_s.contains("Connection: close"));
    }
}
