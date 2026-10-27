//! TCP port forwards: a listener on the sandbox's gateway that relays each
//! connection to a host-side service (`sandbox create --forward`).
//!
//! Unlike the proxy, a forward may target private addresses (a database on
//! the Mac or the LAN): naming the exact host and port is the permission.
//! Only the sandbox's own IP is served, and every connection is logged.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use sandbox_core::api::Forward;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::proxy::Ctx;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Parse forward specs and give each a gateway port from `range`: the one
/// asked for, else `first + port % size` (5432 -> 15432), else the next free.
pub(crate) fn assign(range: &RangeInclusive<u16>, existing: &[Forward], specs: &[String]) -> Result<Vec<Forward>> {
    let (first, size) = (*range.start() as u32, (*range.end() - *range.start()) as u32 + 1);
    let mut taken: HashSet<u16> = existing.iter().map(|f| f.listen).collect();
    let mut out: Vec<Forward> = Vec::new();
    for spec in specs {
        let mut f = Forward::parse(spec)?;
        let duplicate = existing.iter().chain(&out).any(|o| o.host == f.host && o.port == f.port);
        if duplicate && f.listen == 0 {
            continue; // same target already forwarded (e.g. preset + flag)
        }
        if f.listen == 0 {
            let start = f.port as u32 % size;
            f.listen = (0..size)
                .map(|i| (first + (start + i) % size) as u16)
                .find(|p| !taken.contains(p))
                .ok_or_else(|| anyhow!("no free forward ports left in {}-{}", range.start(), range.end()))?;
        } else if !range.contains(&f.listen) {
            bail!("forward port {} is outside forward_ports ({}-{})", f.listen, range.start(), range.end());
        } else if taken.contains(&f.listen) {
            bail!("forward port {} is already in use", f.listen);
        }
        taken.insert(f.listen);
        out.push(f);
    }
    Ok(out)
}

pub(crate) async fn spawn(gateway: IpAddr, fwd: Forward, ctx: Arc<Ctx>) -> Result<JoinHandle<()>> {
    let addr = SocketAddr::new(gateway, fwd.listen);
    let listener = TcpListener::bind(addr).await.with_context(|| format!("bind forward on {addr}"))?;
    tracing::info!(%addr, target = %fwd.target(), "forward listening");
    Ok(tokio::spawn(serve(listener, fwd, ctx)))
}

async fn serve(listener: TcpListener, fwd: Forward, ctx: Arc<Ctx>) {
    let addr = listener.local_addr().ok();
    let fwd = Arc::new(fwd);
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(?addr, error = %e, "forward accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if peer.ip() != ctx.client_ip {
            tracing::warn!(%peer, expected = %ctx.client_ip, "rejecting forward client");
            continue;
        }
        let (fwd, ctx) = (fwd.clone(), ctx.clone());
        tokio::spawn(async move { relay(stream, peer, &fwd, &ctx).await });
    }
}

async fn relay(mut client: TcpStream, peer: SocketAddr, fwd: &Forward, ctx: &Ctx) {
    let connect = TcpStream::connect((fwd.host.as_str(), fwd.port));
    let mut upstream = match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return ctx.record_forward("error", peer, fwd, Some(&e.to_string()), None),
        Err(_) => return ctx.record_forward("error", peer, fwd, Some("connect timed out"), None),
    };
    ctx.record_forward("open", peer, fwd, None, None);
    let started = Instant::now();
    let (up, down) = tokio::io::copy_bidirectional(&mut client, &mut upstream).await.unwrap_or((0, 0));
    ctx.record_forward("close", peer, fwd, None, Some((up, down, started.elapsed().as_millis())));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Policy;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn specs(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn assigns_memorable_ports() {
        let r = 15000..=15999;
        let got = assign(&r, &[], &specs(&["localhost:5432", "10.0.1.20:6379", "15100=db:5432"])).unwrap();
        let ports: Vec<u16> = got.iter().map(|f| f.listen).collect();
        assert_eq!(ports, [15432, 15379, 15100]);

        // Collisions move to the next free port; repeats of a target are dropped.
        let more = assign(&r, &got, &specs(&["other:5432", "localhost:5432"])).unwrap();
        assert_eq!(more.len(), 1);
        assert_eq!(more[0].listen, 15433);

        assert!(assign(&r, &got, &specs(&["15432=x:1"])).is_err(), "taken");
        assert!(assign(&r, &[], &specs(&["8080=x:1"])).is_err(), "outside range");
        let one = 15000..=15000;
        let full = assign(&one, &[], &specs(&["a:1"])).unwrap();
        assert!(assign(&one, &full, &specs(&["b:2"])).is_err(), "full");
    }

    #[tokio::test]
    async fn relays_and_logs() {
        let echo = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = echo.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut s, _) = echo.accept().await.unwrap();
            let mut buf = [0u8; 5];
            s.read_exact(&mut buf).await.unwrap();
            s.write_all(&buf).await.unwrap();
        });

        let log = std::env::temp_dir().join(format!("sandboxd-fwd-{}.jsonl", std::process::id()));
        let ctx = Arc::new(Ctx::for_test("127.0.0.1".parse().unwrap(), Policy::default(), &log));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen = listener.local_addr().unwrap();
        let fwd = Forward { listen: listen.port(), host: "127.0.0.1".into(), port: target.port() };
        let task = tokio::spawn(serve(listener, fwd, ctx));

        let mut c = TcpStream::connect(listen).await.unwrap();
        c.write_all(b"hello").await.unwrap();
        let mut buf = [0u8; 5];
        c.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello");
        drop(c);
        tokio::time::sleep(Duration::from_millis(100)).await;

        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains(r#""event":"open""#) && text.contains(r#""method":"FORWARD""#), "{text}");
        task.abort();
        let _ = std::fs::remove_file(&log);
    }
}
