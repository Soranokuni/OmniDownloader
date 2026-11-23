//! The capture's own egress proxy (P11.15b2): every connection the browser
//! makes goes through it (`--proxy-server`, loopback included), so the egress
//! rule judges each one before it is opened, whatever opened it: a page, a
//! popup, a worker, a WebSocket, a preconnect. The proxy resolves the name
//! itself and connects to the address it checked; the browser's own resolver
//! is never used, so a name that answers public to the rule and internal a
//! moment later (DNS rebinding) cannot take the browser inside.
//!
//! `CONNECT host:port` (https, wss, and ws through a proxy) is tunnelled; a
//! plain http request is sent on with `Connection: close`, so one connection
//! carries one request to one checked host. Anything else is refused.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::task::{JoinHandle, JoinSet};

use omni_core::net::CidrSet;
use omni_core::urlnorm::site_for_log;

/// The longest request head read from the browser.
const MAX_HEAD: usize = 32 * 1024;

/// How long the browser has to send a request head, and the proxy to reach the host.
const HEAD_WAIT: Duration = Duration::from_secs(30);
const CONNECT_WAIT: Duration = Duration::from_secs(10);

/// A running proxy: stopped, with every connection it carries, when dropped.
pub(crate) struct EgressProxy {
    pub addr: SocketAddr,
    /// The site of every refused connection, in order.
    pub refused: UnboundedReceiver<String>,
    task: JoinHandle<()>,
}

impl Drop for EgressProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Start the proxy on 127.0.0.1 under the egress rule (`allow`, `resolve`).
pub(crate) async fn start<R, Fut>(allow: CidrSet, resolve: R, lookup: Duration) -> std::io::Result<EgressProxy>
where
    R: Fn(String, u16) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = std::io::Result<Vec<SocketAddr>>> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let (tx, refused) = unbounded_channel();
    let task = tokio::spawn(async move {
        // Dropping the set (when the task is aborted) aborts every connection.
        let mut connections = JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok((client, _)) => {
                        connections.spawn(serve(client, allow.clone(), resolve.clone(), lookup, tx.clone()));
                    }
                    Err(_) => tokio::time::sleep(Duration::from_millis(50)).await,
                },
                Some(_) = connections.join_next() => {}
            }
        }
    });
    Ok(EgressProxy { addr, refused, task })
}

/// One request head: its first line's parts, the header lines, and the bytes after it.
struct Head {
    method: String,
    target: String,
    version: String,
    headers: Vec<String>,
    rest: Vec<u8>,
}

async fn read_head(client: &mut TcpStream) -> Option<Head> {
    let mut buf = Vec::with_capacity(4096);
    let end = loop {
        if let Some(at) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        if buf.len() > MAX_HEAD {
            return None;
        }
        let mut chunk = [0u8; 4096];
        let n = tokio::time::timeout(HEAD_WAIT, client.read(&mut chunk)).await.ok()?.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let text = std::str::from_utf8(&buf[..end]).ok()?;
    let mut lines = text.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let (method, target, version) = (first.next()?.to_string(), first.next()?.to_string(), first.next()?.to_string());
    let headers = lines.map(str::to_string).collect();
    Some(Head { method, target, version, headers, rest: buf[end + 4..].to_vec() })
}

/// The address the rule judges for a request: `https://host:port/` for a
/// tunnel, the request's own address for plain http. `None`: not something a
/// proxy carries (an origin-form request, another scheme).
fn judged_url(head: &Head) -> Option<String> {
    if head.method.eq_ignore_ascii_case("CONNECT") {
        let authority = head.target.as_str();
        return (!authority.is_empty() && !authority.contains('/') && !authority.contains('@')).then(|| format!("https://{authority}/"));
    }
    head.target.to_ascii_lowercase().starts_with("http://").then(|| head.target.clone())
}

/// The plain http request as the host should see it: origin-form, one request per connection.
fn rewritten(head: &Head, url: &reqwest::Url) -> Vec<u8> {
    let path = match url.query() {
        Some(q) => format!("{}?{q}", url.path()),
        None => url.path().to_string(),
    };
    let mut out = format!("{} {path} {}\r\n", head.method, head.version);
    for h in &head.headers {
        let name = h.split(':').next().unwrap_or_default().trim().to_ascii_lowercase();
        if matches!(name.as_str(), "proxy-connection" | "connection" | "keep-alive" | "proxy-authorization") {
            continue;
        }
        out.push_str(h);
        out.push_str("\r\n");
    }
    out.push_str("Connection: close\r\n\r\n");
    let mut bytes = out.into_bytes();
    bytes.extend_from_slice(&head.rest);
    bytes
}

async fn refuse(client: &mut TcpStream, status: &str) {
    let _ = client.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes()).await;
    let _ = client.shutdown().await;
}

async fn serve<R, Fut>(mut client: TcpStream, allow: CidrSet, resolve: R, lookup: Duration, refused: UnboundedSender<String>)
where
    R: Fn(String, u16) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = std::io::Result<Vec<SocketAddr>>> + Send + 'static,
{
    let Some(head) = read_head(&mut client).await else { return };
    let Some(url) = judged_url(&head) else {
        return refuse(&mut client, "400 Bad Request").await;
    };
    let Ok(parsed) = reqwest::Url::parse(&url) else {
        return refuse(&mut client, "400 Bad Request").await;
    };
    let port = parsed.port_or_known_default().unwrap_or(80);
    // Fails closed: a name that does not resolve, or not in time, is refused.
    let checked = match omni_core::net::check_public_with(&url, &allow, lookup, resolve).await {
        Ok(c) => c,
        Err(_) => {
            let _ = refused.send(site_for_log(&url));
            return refuse(&mut client, "403 Forbidden").await;
        }
    };
    // The addresses the rule checked, and no others: an IP literal is its own.
    let addrs: Vec<SocketAddr> = if checked.addrs.is_empty() {
        let host = parsed.host_str().unwrap_or_default().trim_start_matches('[').trim_end_matches(']');
        host.parse().ok().map(|ip| vec![SocketAddr::new(ip, port)]).unwrap_or_default()
    } else {
        checked.addrs.iter().map(|a| SocketAddr::new(a.ip(), port)).collect()
    };
    let mut upstream = None;
    for addr in addrs {
        if let Ok(Ok(s)) = tokio::time::timeout(CONNECT_WAIT, TcpStream::connect(addr)).await {
            upstream = Some(s);
            break;
        }
    }
    let Some(mut upstream) = upstream else {
        return refuse(&mut client, "502 Bad Gateway").await;
    };
    let sent = if head.method.eq_ignore_ascii_case("CONNECT") {
        match client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await {
            Ok(()) => upstream.write_all(&head.rest).await,
            Err(e) => Err(e),
        }
    } else {
        upstream.write_all(&rewritten(&head, &parsed)).await
    };
    if sent.is_ok() {
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(raw: &str) -> Head {
        let (h, rest) = raw.split_once("\r\n\r\n").unwrap();
        let mut lines = h.split("\r\n");
        let mut first = lines.next().unwrap().split(' ');
        Head {
            method: first.next().unwrap().into(),
            target: first.next().unwrap().into(),
            version: first.next().unwrap().into(),
            headers: lines.map(str::to_string).collect(),
            rest: rest.as_bytes().to_vec(),
        }
    }

    #[test]
    fn a_tunnel_is_judged_by_its_host_and_port_and_plain_http_by_its_address() {
        assert_eq!(judged_url(&head("CONNECT files.example:443 HTTP/1.1\r\nHost: files.example:443\r\n\r\n")).as_deref(), Some("https://files.example:443/"));
        assert_eq!(judged_url(&head("CONNECT [::1]:80 HTTP/1.1\r\n\r\n")).as_deref(), Some("https://[::1]:80/"));
        assert_eq!(judged_url(&head("GET http://127.0.0.1:81/file?a=1 HTTP/1.1\r\n\r\n")).as_deref(), Some("http://127.0.0.1:81/file?a=1"));
        assert_eq!(judged_url(&head("GET /file HTTP/1.1\r\n\r\n")), None, "not a proxy request");
        assert_eq!(judged_url(&head("GET ftp://files.example/x HTTP/1.1\r\n\r\n")), None);
        assert_eq!(judged_url(&head("CONNECT user@files.example:443 HTTP/1.1\r\n\r\n")), None);
    }

    #[test]
    fn a_plain_request_goes_on_in_origin_form_one_per_connection() {
        let h = head("GET http://127.0.0.1:81/file?a=1 HTTP/1.1\r\nHost: 127.0.0.1:81\r\nProxy-Connection: keep-alive\r\nConnection: keep-alive\r\nAccept: */*\r\n\r\nBODY");
        let url = reqwest::Url::parse(&h.target).unwrap();
        let out = String::from_utf8(rewritten(&h, &url)).unwrap();
        assert_eq!(out, "GET /file?a=1 HTTP/1.1\r\nHost: 127.0.0.1:81\r\nAccept: */*\r\nConnection: close\r\n\r\nBODY");
    }

    async fn ask(proxy: SocketAddr, request: &str) -> String {
        let mut s = TcpStream::connect(proxy).await.unwrap();
        s.write_all(request.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
        String::from_utf8_lossy(&out).into_owned()
    }

    #[tokio::test]
    async fn the_proxy_refuses_what_the_rule_refuses_and_connects_to_what_it_checked() {
        // A host on 127.0.0.1 that answers one request.
        let host = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host_addr = host.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut c, _)) = host.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 2048];
                    let _ = c.read(&mut buf).await;
                    let _ = c.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok").await;
                });
            }
        });
        // `admitted.test` resolves to that host; every other name fails.
        let resolve = |name: String, port: u16| async move {
            if name == "admitted.test" {
                Ok(vec![SocketAddr::from(([127, 0, 0, 1], port))])
            } else {
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no lookups in tests"))
            }
        };
        let allow = CidrSet::parse(&["127.0.0.1/32".to_string()]).unwrap();
        let mut proxy = start(allow, resolve, Duration::from_secs(2)).await.unwrap();
        let port = host_addr.port();

        let ok = ask(proxy.addr, &format!("GET http://admitted.test:{port}/x HTTP/1.1\r\nHost: admitted.test\r\n\r\n")).await;
        assert!(ok.starts_with("HTTP/1.1 200") && ok.ends_with("ok"), "{ok}");
        let tunnel = ask(proxy.addr, &format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\n\r\nGET / HTTP/1.1\r\n\r\n")).await;
        assert!(tunnel.starts_with("HTTP/1.1 200 Connection Established"), "{tunnel}");

        let internal = ask(proxy.addr, &format!("CONNECT 127.0.0.2:{port} HTTP/1.1\r\n\r\n")).await;
        assert!(internal.starts_with("HTTP/1.1 403"), "{internal}");
        let unknown = ask(proxy.addr, "GET http://nowhere.test/x HTTP/1.1\r\n\r\n").await;
        assert!(unknown.starts_with("HTTP/1.1 403"), "fails closed: {unknown}");
        let origin = ask(proxy.addr, "GET /x HTTP/1.1\r\n\r\n").await;
        assert!(origin.starts_with("HTTP/1.1 400"), "{origin}");
        assert_eq!(proxy.refused.recv().await.as_deref(), Some("127.0.0.2"));
        assert_eq!(proxy.refused.recv().await.as_deref(), Some("nowhere.test"));
    }
}
