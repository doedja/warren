//! End-to-end: client -> hub (HTTP CONNECT) -> node -> target, bytes round-trip.
//!
//! Boots a target echo server, a hub on ephemeral ports, and one node agent,
//! then drives a real CONNECT request through the hub and asserts the payload
//! comes back, proving it traversed client -> hub -> node -> target -> back.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use warren::hub::{run_with_listeners, HubConfig};
use warren::node::{run_agent, RunArgs};

#[tokio::test]
async fn end_to_end_proxy_through_node() {
    // 1. Target echo server: echoes the first 4 bytes of each connection.
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_addr = target.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = target.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4];
                if s.read_exact(&mut buf).await.is_ok() {
                    let _ = s.write_all(&buf).await;
                    let _ = s.flush().await;
                }
            });
        }
    });

    // 2. Hub on ephemeral ports.
    let node_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let node_addr = node_l.local_addr().unwrap();
    let proxy_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_l.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = run_with_listeners(node_l, proxy_l, {
            let store = std::sync::Arc::new(warren::store::Store::open(":memory:").unwrap());
            store.add_token("secret", "test").unwrap();
            HubConfig {
                store,
                tls: None,
                admin: None,
                public_node_addr: None,
                public_proxy_addr: None,
                fingerprint: None,
            }
        })
        .await;
    });

    // 3. One node agent dialing the hub.
    tokio::spawn(async move {
        let _ = run_agent(RunArgs {
            join: None,
            hub: Some(node_addr.to_string()),
            token: Some("secret".into()),
            key_file: Some(format!(
                "{}/warren-e2e-proxy.key",
                std::env::temp_dir().display()
            )),
            name: "test-node".into(),
            tls: false,
            hub_fingerprint: None,
            insecure: false,
        })
        .await;
    });

    // 4. Client request through the hub. Retry until the node has enrolled
    //    (before that the hub has no node and returns 502).
    let mut last = String::from("never attempted");
    for _ in 0..50 {
        match try_proxy(proxy_addr, target_addr).await {
            Ok(true) => return,
            Ok(false) => last = "non-200 response or wrong echo".to_string(),
            Err(e) => last = e.to_string(),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("end-to-end proxy never succeeded: {last}");
}

/// Returns Ok(true) on a full successful round-trip through the pool.
async fn try_proxy(proxy_addr: SocketAddr, target_addr: SocketAddr) -> std::io::Result<bool> {
    let mut c = TcpStream::connect(proxy_addr).await?;
    let req = format!("CONNECT {target_addr} HTTP/1.1\r\nHost: {target_addr}\r\n\r\n");
    c.write_all(req.as_bytes()).await?;
    c.flush().await?;

    // Read the response head up to the blank line.
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        c.read_exact(&mut byte).await?;
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") || head.len() > 1024 {
            break;
        }
    }
    if !String::from_utf8_lossy(&head).contains(" 200 ") {
        return Ok(false);
    }

    // Tunnel established: send a marker, expect it echoed back by the target.
    c.write_all(b"PING").await?;
    c.flush().await?;
    let mut buf = [0u8; 4];
    c.read_exact(&mut buf).await?;
    Ok(&buf == b"PING")
}

#[tokio::test]
async fn rejects_bad_auth_and_allows_good() {
    // Target echo server.
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_addr = target.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = target.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4];
                if s.read_exact(&mut buf).await.is_ok() {
                    let _ = s.write_all(&buf).await;
                    let _ = s.flush().await;
                }
            });
        }
    });

    // Hub requiring Basic auth u:p.
    let node_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let node_addr = node_l.local_addr().unwrap();
    let proxy_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_l.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = run_with_listeners(node_l, proxy_l, {
            let store = std::sync::Arc::new(warren::store::Store::open(":memory:").unwrap());
            store.add_token("secret", "test").unwrap();
            store.add_user("u", "p").unwrap();
            HubConfig {
                store,
                tls: None,
                admin: None,
                public_node_addr: None,
                public_proxy_addr: None,
                fingerprint: None,
            }
        })
        .await;
    });
    tokio::spawn(async move {
        let _ = run_agent(RunArgs {
            join: None,
            hub: Some(node_addr.to_string()),
            token: Some("secret".into()),
            key_file: Some(format!(
                "{}/warren-e2e-auth.key",
                std::env::temp_dir().display()
            )),
            name: "auth-node".into(),
            tls: false,
            hub_fingerprint: None,
            insecure: false,
        })
        .await;
    });

    // Wrong creds are rejected with 407 (auth is checked before node lookup,
    // so this holds even before the node enrolls).
    let (status, _) = proxy_attempt(proxy_addr, target_addr, Some(("u", "wrong")))
        .await
        .expect("attempt");
    assert_eq!(status, 407, "bad creds must be rejected");

    // Correct creds succeed once the node is up.
    for _ in 0..50 {
        if let Ok((200, true)) = proxy_attempt(proxy_addr, target_addr, Some(("u", "p"))).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("authorized request never succeeded");
}

#[tokio::test]
async fn routes_to_named_device_and_rejects_unknown() {
    // Target echo server.
    let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_addr = target.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = target.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 4];
                if s.read_exact(&mut buf).await.is_ok() {
                    let _ = s.write_all(&buf).await;
                    let _ = s.flush().await;
                }
            });
        }
    });

    // Hub requiring Basic auth, with one device named "exit1".
    let node_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let node_addr = node_l.local_addr().unwrap();
    let proxy_l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_l.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = run_with_listeners(node_l, proxy_l, {
            let store = std::sync::Arc::new(warren::store::Store::open(":memory:").unwrap());
            store.add_token("secret", "test").unwrap();
            store.add_user("u", "p").unwrap();
            HubConfig {
                store,
                tls: None,
                admin: None,
                public_node_addr: None,
                public_proxy_addr: None,
                fingerprint: None,
            }
        })
        .await;
    });
    tokio::spawn(async move {
        let _ = run_agent(RunArgs {
            join: None,
            hub: Some(node_addr.to_string()),
            token: Some("secret".into()),
            key_file: Some(format!(
                "{}/warren-e2e-named.key",
                std::env::temp_dir().display()
            )),
            name: "exit1".into(),
            tls: false,
            hub_fingerprint: None,
            insecure: false,
        })
        .await;
    });

    // Once the named device is reachable via `u+exit1`, the pin works.
    let mut up = false;
    for _ in 0..50 {
        if let Ok((200, true)) =
            proxy_attempt(proxy_addr, target_addr, Some(("u+exit1", "p"))).await
        {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(up, "request pinned to the named device never succeeded");

    // A device that does not exist is rejected (502), never silently served by
    // another device.
    let (status, _) = proxy_attempt(proxy_addr, target_addr, Some(("u+ghost", "p")))
        .await
        .expect("attempt");
    assert_eq!(
        status, 502,
        "unknown device name must not fall back to the pool"
    );
}

/// Returns (http_status, echo_ok). echo_ok is only meaningful on 200.
async fn proxy_attempt(
    proxy_addr: SocketAddr,
    target_addr: SocketAddr,
    auth: Option<(&str, &str)>,
) -> std::io::Result<(u16, bool)> {
    use base64::Engine;
    let mut c = TcpStream::connect(proxy_addr).await?;
    let mut req = format!("CONNECT {target_addr} HTTP/1.1\r\nHost: {target_addr}\r\n");
    if let Some((u, p)) = auth {
        let creds = base64::engine::general_purpose::STANDARD.encode(format!("{u}:{p}"));
        req.push_str(&format!("Proxy-Authorization: Basic {creds}\r\n"));
    }
    req.push_str("\r\n");
    c.write_all(req.as_bytes()).await?;
    c.flush().await?;

    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        c.read_exact(&mut byte).await?;
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") || head.len() > 1024 {
            break;
        }
    }
    let head_s = String::from_utf8_lossy(&head);
    let status: u16 = head_s
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if status != 200 {
        return Ok((status, false));
    }

    c.write_all(b"PING").await?;
    c.flush().await?;
    let mut buf = [0u8; 4];
    c.read_exact(&mut buf).await?;
    Ok((200, &buf == b"PING"))
}
