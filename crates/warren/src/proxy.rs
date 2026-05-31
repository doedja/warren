//! Client-facing HTTP CONNECT proxy: request parsing + Basic proxy auth.
//!
//! Hand-rolled (no HTTP library): the hub only needs to read a single CONNECT
//! request line plus headers, then splice raw bytes.

use base64::Engine;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const MAX_HEAD: usize = 8192;

#[derive(Debug, Clone)]
pub struct ConnectRequest {
    pub host: String,
    pub port: u16,
    pub authorization: Option<String>,
}

fn invalid(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg)
}

/// Read and parse one `CONNECT host:port HTTP/1.1` request (with headers) from
/// the client. Reads until the `\r\n\r\n` head terminator, capped at MAX_HEAD.
pub async fn read_connect_request<R: AsyncRead + Unpin>(
    r: &mut R,
) -> std::io::Result<ConnectRequest> {
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
    let method = parts.next().ok_or_else(|| invalid("no method"))?;
    let target = parts.next().ok_or_else(|| invalid("no target"))?;
    if !method.eq_ignore_ascii_case("CONNECT") {
        return Err(invalid("not a CONNECT request"));
    }

    let colon = target
        .rfind(':')
        .ok_or_else(|| invalid("no port in target"))?;
    let host = target[..colon].to_string();
    let port: u16 = target[colon + 1..]
        .parse()
        .map_err(|_| invalid("bad port"))?;
    if host.is_empty() {
        return Err(invalid("empty host"));
    }

    let mut authorization = None;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("proxy-authorization") {
                authorization = Some(value.trim().to_string());
            }
        }
    }

    Ok(ConnectRequest {
        host,
        port,
        authorization,
    })
}

/// Validate Basic proxy credentials. `None` expected = no auth required.
pub fn check_proxy_auth(req: &ConnectRequest, expected: Option<&(String, String)>) -> bool {
    let Some((want_user, want_pass)) = expected else {
        return true;
    };
    let Some(auth) = req.authorization.as_deref() else {
        return false;
    };
    // Strip a case-insensitive "Basic " scheme prefix.
    let rest = match auth.get(..6) {
        Some(scheme) if scheme.eq_ignore_ascii_case("basic ") => &auth[6..],
        _ => return false,
    };
    let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(rest.trim()) else {
        return false;
    };
    let Ok(creds) = String::from_utf8(decoded) else {
        return false;
    };
    match creds.split_once(':') {
        Some((user, pass)) => user == want_user && pass == want_pass,
        None => false,
    }
}

/// A parsed HTTP request head (request line + headers), for the plain-HTTP
/// (absolute-URI) proxy path. CONNECT is handled by [`read_connect_request`].
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
    async fn parses_connect_with_auth() {
        let creds = base64::engine::general_purpose::STANDARD.encode("user:pass");
        let raw = format!(
            "CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\nProxy-Authorization: Basic {creds}\r\n\r\n"
        );
        let mut r = std::io::Cursor::new(raw.into_bytes());
        let req = read_connect_request(&mut r).await.unwrap();
        assert_eq!(req.host, "example.com");
        assert_eq!(req.port, 443);
        assert_eq!(req.authorization, Some(format!("Basic {creds}")));
    }

    #[tokio::test]
    async fn auth_matches_and_rejects() {
        let creds = base64::engine::general_purpose::STANDARD.encode("user:pass");
        let req = ConnectRequest {
            host: "h".into(),
            port: 1,
            authorization: Some(format!("Basic {creds}")),
        };
        let want = ("user".to_string(), "pass".to_string());
        assert!(check_proxy_auth(&req, Some(&want)));
        let wrong = ("user".to_string(), "nope".to_string());
        assert!(!check_proxy_auth(&req, Some(&wrong)));
        // No auth configured = always allowed.
        assert!(check_proxy_auth(&req, None));
    }

    #[tokio::test]
    async fn rejects_non_connect() {
        let mut r = std::io::Cursor::new(b"GET / HTTP/1.1\r\n\r\n".to_vec());
        assert!(read_connect_request(&mut r).await.is_err());
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
