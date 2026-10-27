//! Per-sandbox HTTP egress proxy.
//!
//! Each sandbox lives on its own host-only network with no route to the
//! internet. The only thing it can reach is this proxy, bound on the
//! network's gateway address. HTTPS goes through `CONNECT host:port` and plain
//! HTTP through absolute-form requests; both are checked against the
//! sandbox's allowlist and recorded in its egress log.

use std::collections::HashMap;
use std::io::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::policy::{split_host_port, Policy};

const MAX_HEAD: usize = 64 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HEAD_TIMEOUT: Duration = Duration::from_secs(30);

pub struct ProxyHandle {
    pub policy: Arc<RwLock<Policy>>,
    task: JoinHandle<()>,
    dns: JoinHandle<()>,
    ctx: Arc<Ctx>,
    gateway: IpAddr,
    /// Port forward listeners by gateway port.
    forwards: Mutex<HashMap<u16, JoinHandle<()>>>,
}

impl ProxyHandle {
    /// What a forward listener needs: where to bind, and the shared context.
    pub fn forward_context(&self) -> (IpAddr, Arc<Ctx>) {
        (self.gateway, self.ctx.clone())
    }

    pub fn attach_forward(&self, listen: u16, task: JoinHandle<()>) {
        if let Some(old) = self.forwards.lock().unwrap().insert(listen, task) {
            old.abort();
        }
    }

    pub fn detach_forward(&self, listen: u16) {
        if let Some(task) = self.forwards.lock().unwrap().remove(&listen) {
            task.abort();
        }
    }
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        self.task.abort();
        self.dns.abort();
        for (_, task) in self.forwards.lock().unwrap().drain() {
            task.abort();
        }
    }
}

pub struct Ctx {
    /// Only connections from this address are served; anything else on the
    /// gateway (another sandbox, the LAN) is dropped.
    pub(crate) client_ip: IpAddr,
    pub(crate) policy: Arc<RwLock<Policy>>,
    pub(crate) block_private_ips: bool,
    log: Mutex<std::fs::File>,
}

#[derive(Serialize, Clone)]
struct Event<'a> {
    ts: String,
    /// `open` when an allowed connection starts, `close` when it ends (with
    /// byte counts), `deny` or `error` when it never got going.
    event: &'static str,
    client: String,
    method: &'a str,
    host: &'a str,
    port: u16,
    allowed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes_up: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bytes_down: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u128>,
}

impl Ctx {
    fn record(&self, ev: Event<'_>) {
        let Ok(mut line) = serde_json::to_vec(&ev) else { return };
        line.push(b'\n');
        if let Ok(mut f) = self.log.lock() {
            let _ = f.write_all(&line);
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(client_ip: IpAddr, policy: Policy, log: &std::path::Path) -> Self {
        Self {
            client_ip,
            policy: Arc::new(RwLock::new(policy)),
            block_private_ips: false,
            log: Mutex::new(std::fs::File::create(log).unwrap()),
        }
    }

    pub(crate) fn record_forward(
        &self,
        event: &'static str,
        peer: SocketAddr,
        fwd: &sandbox_core::api::Forward,
        reason: Option<&str>,
        totals: Option<(u64, u64, u128)>,
    ) {
        self.record(Event {
            ts: chrono::Utc::now().to_rfc3339(),
            event,
            client: peer.to_string(),
            method: "FORWARD",
            host: &fwd.host,
            port: fwd.port,
            allowed: event != "error",
            reason,
            bytes_up: totals.map(|t| t.0),
            bytes_down: totals.map(|t| t.1),
            duration_ms: totals.map(|t| t.2),
        });
    }

    pub(crate) fn record_dns_deny(&self, peer: SocketAddr, name: &str) {
        self.record(Event {
            ts: chrono::Utc::now().to_rfc3339(),
            event: "deny",
            client: peer.to_string(),
            method: "DNS",
            host: name,
            port: 53,
            allowed: false,
            reason: Some("not in allowlist"),
            bytes_up: None,
            bytes_down: None,
            duration_ms: None,
        });
    }
}

/// Bind the proxy on `gateway:port` and the DNS responder on
/// `gateway:dns_port`, serving only `client_ip`.
///
/// The gateway interface only appears once the first container has joined the
/// network, so binding is retried briefly.
pub async fn spawn(
    gateway: IpAddr,
    port: u16,
    dns_port: u16,
    client_ip: IpAddr,
    policy: Policy,
    block_private_ips: bool,
    log_path: PathBuf,
) -> Result<ProxyHandle> {
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("open egress log {}", log_path.display()))?;

    let addr = SocketAddr::new(gateway, port);
    let mut attempt = 0;
    let listener = loop {
        match TcpListener::bind(addr).await {
            Ok(l) => break l,
            Err(e) if attempt < 40 => {
                attempt += 1;
                tracing::debug!(%addr, error = %e, "proxy bind failed, retrying");
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(e) => return Err(e).with_context(|| format!("bind egress proxy on {addr}")),
        }
    };

    let policy = Arc::new(RwLock::new(policy));
    let ctx = Arc::new(Ctx {
        client_ip,
        policy: policy.clone(),
        block_private_ips,
        log: Mutex::new(log),
    });
    let ctx_shared = ctx.clone();
    let task = tokio::spawn(async move {
        loop {
            let (stream, peer) = match listener.accept().await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(%addr, error = %e, "proxy accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let ctx = ctx.clone();
            tokio::spawn(async move {
                if peer.ip() != ctx.client_ip {
                    tracing::warn!(%peer, expected = %ctx.client_ip, "rejecting proxy client");
                    return;
                }
                if let Err(e) = handle(stream, peer, &ctx).await {
                    tracing::debug!(%peer, error = %e, "proxy connection ended with error");
                }
            });
        }
    });
    let dns = crate::dns::spawn(SocketAddr::new(gateway, dns_port), ctx_shared.clone()).await?;
    tracing::info!(%addr, %client_ip, "egress proxy listening");
    Ok(ProxyHandle {
        policy,
        task,
        dns,
        ctx: ctx_shared,
        gateway,
        forwards: Mutex::new(HashMap::new()),
    })
}

struct Head {
    method: String,
    target: String,
    version: String,
    headers: Vec<(String, String)>,
    /// Bytes read past the end of the head (start of the body or tunnel).
    rest: Vec<u8>,
}

async fn read_head(stream: &mut TcpStream) -> Result<Head> {
    let mut buf = Vec::with_capacity(4096);
    let end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i;
        }
        if buf.len() >= MAX_HEAD {
            bail!("request head too large");
        }
        let mut chunk = [0u8; 4096];
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            bail!("client closed before sending a request");
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let text = std::str::from_utf8(&buf[..end]).context("non-UTF-8 request head")?;
    let mut lines = text.split("\r\n");
    let mut parts = lines.next().unwrap_or_default().split_whitespace();
    let (Some(method), Some(target), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        bail!("malformed request line");
    };
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    Ok(Head {
        method: method.to_string(),
        target: target.to_string(),
        version: version.to_string(),
        headers,
        rest: buf[end + 4..].to_vec(),
    })
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

async fn respond(stream: &mut TcpStream, status: &str, body: &str) -> Result<()> {
    let msg = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(msg.as_bytes()).await?;
    Ok(())
}

async fn handle(mut client: TcpStream, peer: SocketAddr, ctx: &Ctx) -> Result<()> {
    let head = tokio::time::timeout(HEAD_TIMEOUT, read_head(&mut client))
        .await
        .context("timed out waiting for request head")??;

    // Work out where the client wants to go, and what to send upstream first.
    let (host, port, preamble, is_connect) = if head.method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = split_host_port(&head.target)?;
        (host.to_string(), port.unwrap_or(443), Vec::new(), true)
    } else if let Some(rest) = head.target.strip_prefix("http://") {
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        let (host, port) = split_host_port(authority)?;
        let mut out = format!("{} {} {}\r\n", head.method, path, head.version);
        for (k, v) in &head.headers {
            if k.eq_ignore_ascii_case("proxy-connection") || k.eq_ignore_ascii_case("proxy-authorization") {
                continue;
            }
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        out.push_str("\r\n");
        (host.to_string(), port.unwrap_or(80), out.into_bytes(), false)
    } else {
        respond(
            &mut client,
            "400 Bad Request",
            "sandbox egress proxy: send CONNECT host:port or an absolute http:// URL\n",
        )
        .await?;
        return Ok(());
    };

    let mut ev = Event {
        ts: chrono::Utc::now().to_rfc3339(),
        event: "deny",
        client: peer.to_string(),
        method: &head.method,
        host: &host,
        port,
        allowed: false,
        reason: None,
        bytes_up: None,
        bytes_down: None,
        duration_ms: None,
    };

    let allowed = ctx.policy.read().map(|p| p.allows(&host, port)).unwrap_or(false);
    if !allowed {
        ev.reason = Some("not in allowlist");
        ctx.record(ev);
        let body = format!(
            "sandbox: egress to {host}:{port} is blocked by policy.\n\
             Allow it from the host with: sandbox allow <id> {host}\n"
        );
        respond(&mut client, "403 Forbidden", &body).await?;
        return Ok(());
    }

    let upstream = match connect_upstream(&host, port, ctx.block_private_ips).await {
        Ok(s) => s,
        Err(e) => {
            let reason = e.to_string();
            ev.event = "error";
            ev.reason = Some(&reason);
            ctx.record(ev);
            respond(&mut client, "502 Bad Gateway", &format!("sandbox: {reason}\n")).await?;
            return Ok(());
        }
    };
    let mut upstream = upstream;
    ev.event = "open";
    ev.allowed = true;
    ctx.record(ev.clone());

    if is_connect {
        client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n").await?;
    } else {
        upstream.write_all(&preamble).await?;
    }
    if !head.rest.is_empty() {
        upstream.write_all(&head.rest).await?;
    }

    let started = Instant::now();
    let result = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    let (up, down) = result.unwrap_or((0, 0));
    ev.ts = chrono::Utc::now().to_rfc3339();
    ev.event = "close";
    ev.bytes_up = Some(up + (preamble.len() + head.rest.len()) as u64);
    ev.bytes_down = Some(down);
    ev.duration_ms = Some(started.elapsed().as_millis());
    ctx.record(ev);
    Ok(())
}

async fn connect_upstream(host: &str, port: u16, block_private: bool) -> Result<TcpStream> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("cannot resolve {host}"))?
        .collect();
    let usable: Vec<SocketAddr> = addrs
        .into_iter()
        .filter(|a| !block_private || is_public(a.ip()))
        .collect();
    if usable.is_empty() {
        bail!("{host} resolves only to private or loopback addresses");
    }
    let mut last_err = None;
    for addr in usable {
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(s)) => return Ok(s),
            Ok(Err(e)) => last_err = Some(anyhow::Error::from(e)),
            Err(_) => last_err = Some(anyhow::anyhow!("connect to {addr} timed out")),
        }
    }
    Err(last_err.unwrap()).with_context(|| format!("cannot connect to {host}:{port}"))
}

pub(crate) fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || o[0] == 0
                || (o[0] == 100 && (o[1] & 0xc0) == 64)) // CGNAT 100.64/10
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_public(IpAddr::V4(v4));
            }
            let seg = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg[0] & 0xfe00) == 0xfc00 // unique local
                || (seg[0] & 0xffc0) == 0xfe80) // link local
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_ip_filter() {
        for ip in ["127.0.0.1", "10.0.0.1", "192.168.64.1", "172.16.0.1", "169.254.1.1", "100.100.1.1", "::1", "fd00::1", "fe80::1", "::ffff:10.0.0.1"] {
            assert!(!is_public(ip.parse().unwrap()), "{ip} should be private");
        }
        for ip in ["1.1.1.1", "140.82.112.3", "2606:4700::1111"] {
            assert!(is_public(ip.parse().unwrap()), "{ip} should be public");
        }
    }
}
