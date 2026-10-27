//! Allowlist-scoped DNS for a sandbox, on its gateway. The pf anchor from
//! scripts/firewall.sh redirects the sandbox's port 53 here (`dns_port`).
//!
//! Sandboxes have no route to a real resolver. Tools that resolve a name
//! before connecting (SSRF guards, some HTTP clients) would fail even though
//! the proxy would let the connection through. This answers A/AAAA for
//! allowlisted names with the addresses the host resolves, and NXDOMAIN for
//! everything else. Disallowed names are never forwarded, so queries can't be
//! used to smuggle data out, and the answers don't open any route: traffic
//! still has to go through the proxy.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

use crate::proxy::{is_public, Ctx};
#[cfg(test)]
use crate::policy::Policy;

const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
const TTL: u32 = 30;
const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;
const CLASS_IN: u16 = 1;
const RCODE_FORMERR: u8 = 1;
const RCODE_SERVFAIL: u8 = 2;
const RCODE_NXDOMAIN: u8 = 3;
const RCODE_NOTIMP: u8 = 4;

pub(crate) async fn spawn(addr: SocketAddr, ctx: Arc<Ctx>) -> Result<JoinHandle<()>> {
    let sock = UdpSocket::bind(addr).await.with_context(|| format!("bind DNS on {addr}"))?;
    Ok(tokio::spawn(serve(sock, ctx)))
}

async fn serve(sock: UdpSocket, ctx: Arc<Ctx>) {
    let addr = sock.local_addr().ok();
    let sock = Arc::new(sock);
    let mut buf = [0u8; 1500];
    loop {
        let (n, peer) = match sock.recv_from(&mut buf).await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(?addr, error = %e, "dns recv failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        if peer.ip() != ctx.client_ip {
            tracing::debug!(%peer, "dropping DNS query from unknown client");
            continue;
        }
        let (query, sock, ctx) = (buf[..n].to_vec(), sock.clone(), ctx.clone());
        tracing::trace!(%peer, len = n, "dns query");
        tokio::spawn(async move {
            if let Some(reply) = answer(&query, peer, &ctx).await {
                let _ = sock.send_to(&reply, peer).await;
            }
        });
    }
}

struct Question {
    name: String,
    qtype: u16,
    qclass: u16,
    /// Offset just past the question, so it can be echoed verbatim.
    end: usize,
}

async fn answer(query: &[u8], peer: SocketAddr, ctx: &Ctx) -> Option<Vec<u8>> {
    if query.len() < 12 || query[2] & 0x80 != 0 {
        return None; // too short, or a response
    }
    let opcode = (query[2] >> 3) & 0x0f;
    let qdcount = u16::from_be_bytes([query[4], query[5]]);
    if opcode != 0 || qdcount != 1 {
        return Some(reply(query, 12, RCODE_NOTIMP, &[]));
    }
    let Some(q) = parse_question(query) else {
        return Some(reply(query, 12, RCODE_FORMERR, &[]));
    };
    let allowed = ctx.policy.read().map(|p| p.allows_name(&q.name)).unwrap_or(false);
    if !allowed {
        // AAAA queries usually come paired with A; log each name once.
        if q.qtype != TYPE_AAAA {
            ctx.record_dns_deny(peer, &q.name);
        }
        return Some(reply(query, q.end, RCODE_NXDOMAIN, &[]));
    }
    if q.qclass != CLASS_IN || (q.qtype != TYPE_A && q.qtype != TYPE_AAAA) {
        return Some(reply(query, q.end, 0, &[])); // name exists, no such record
    }
    let addrs = match tokio::time::timeout(LOOKUP_TIMEOUT, tokio::net::lookup_host((q.name.as_str(), 0))).await {
        Ok(Ok(addrs)) => addrs,
        Ok(Err(_)) => return Some(reply(query, q.end, RCODE_NXDOMAIN, &[])),
        Err(_) => return Some(reply(query, q.end, RCODE_SERVFAIL, &[])),
    };
    let mut ips: Vec<IpAddr> = addrs
        .map(|a| a.ip())
        .filter(|ip| ip.is_ipv4() == (q.qtype == TYPE_A))
        .filter(|ip| !ctx.block_private_ips || is_public(*ip))
        .collect();
    ips.dedup();
    Some(reply(query, q.end, 0, &ips))
}

fn parse_question(msg: &[u8]) -> Option<Question> {
    let mut pos = 12;
    let mut labels = Vec::new();
    loop {
        let len = *msg.get(pos)? as usize;
        pos += 1;
        if len == 0 {
            break;
        }
        if len & 0xc0 != 0 {
            return None; // compression pointers don't belong in a question
        }
        let label = msg.get(pos..pos + len)?;
        labels.push(std::str::from_utf8(label).ok()?.to_ascii_lowercase());
        pos += len;
    }
    let fixed = msg.get(pos..pos + 4)?;
    Some(Question {
        name: labels.join("."),
        qtype: u16::from_be_bytes([fixed[0], fixed[1]]),
        qclass: u16::from_be_bytes([fixed[2], fixed[3]]),
        end: pos + 4,
    })
}

/// Build a response echoing the header id and the question up to `qend`.
fn reply(query: &[u8], qend: usize, rcode: u8, ips: &[IpAddr]) -> Vec<u8> {
    let has_question = qend > 12;
    let mut out = Vec::with_capacity(qend + ips.len() * 28);
    out.extend_from_slice(&query[..2]); // id
    out.push(0x80 | (query[2] & 0x79)); // QR, opcode, RD
    out.push(0x80 | rcode); // RA
    out.extend_from_slice(&(has_question as u16).to_be_bytes());
    out.extend_from_slice(&(ips.len() as u16).to_be_bytes());
    out.extend_from_slice(&[0, 0, 0, 0]); // no authority or additional records
    out.extend_from_slice(&query[12..qend]);
    for ip in ips {
        out.extend_from_slice(&[0xc0, 0x0c]); // name: pointer to the question
        let (rtype, data) = match ip {
            IpAddr::V4(v4) => (TYPE_A, v4.octets().to_vec()),
            IpAddr::V6(v6) => (TYPE_AAAA, v6.octets().to_vec()),
        };
        out.extend_from_slice(&rtype.to_be_bytes());
        out.extend_from_slice(&CLASS_IN.to_be_bytes());
        out.extend_from_slice(&TTL.to_be_bytes());
        out.extend_from_slice(&(data.len() as u16).to_be_bytes());
        out.extend_from_slice(&data);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(name: &str, qtype: u16) -> Vec<u8> {
        let mut q = vec![0x12, 0x34, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            q.push(label.len() as u8);
            q.extend_from_slice(label.as_bytes());
        }
        q.push(0);
        q.extend_from_slice(&qtype.to_be_bytes());
        q.extend_from_slice(&CLASS_IN.to_be_bytes());
        q
    }

    #[test]
    fn parses_question() {
        let q = query("Api.Example.com", TYPE_AAAA);
        let parsed = parse_question(&q).unwrap();
        assert_eq!(parsed.name, "api.example.com");
        assert_eq!(parsed.qtype, TYPE_AAAA);
        assert_eq!(parsed.end, q.len());
        assert!(parse_question(&q[..q.len() - 2]).is_none());
    }

    #[test]
    fn builds_answers() {
        let q = query("example.com", TYPE_A);
        let ip: IpAddr = "93.184.215.14".parse().unwrap();
        let r = reply(&q, q.len(), 0, &[ip]);
        assert_eq!(&r[..2], &[0x12, 0x34]);
        assert_eq!(r[2] & 0x80, 0x80); // response
        assert_eq!(r[2] & 0x01, 0x01); // RD echoed
        assert_eq!(r[3] & 0x0f, 0); // NOERROR
        assert_eq!(u16::from_be_bytes([r[6], r[7]]), 1); // one answer
        assert_eq!(&r[r.len() - 4..], &[93, 184, 215, 14]);

        let nx = reply(&q, q.len(), RCODE_NXDOMAIN, &[]);
        assert_eq!(nx[3] & 0x0f, RCODE_NXDOMAIN);
        assert_eq!(nx.len(), q.len());
    }

    #[tokio::test]
    async fn answers_only_allowlisted_names() {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server = sock.local_addr().unwrap();
        let policy = Policy::new(&["localhost".into()]).unwrap();
        let log = std::env::temp_dir().join(format!("sandboxd-dns-{}.jsonl", std::process::id()));
        let ctx = Arc::new(Ctx::for_test("127.0.0.1".parse().unwrap(), policy, &log));
        let task = tokio::spawn(serve(sock, ctx));

        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let ask = |name: &'static str| {
            let client = &client;
            async move {
                client.send_to(&query(name, TYPE_A), server).await.unwrap();
                let mut buf = [0u8; 512];
                let n = tokio::time::timeout(Duration::from_secs(5), client.recv(&mut buf)).await.unwrap().unwrap();
                buf[..n].to_vec()
            }
        };
        let ok = ask("localhost").await;
        assert_eq!(ok[3] & 0x0f, 0);
        assert_eq!(&ok[ok.len() - 4..], &[127, 0, 0, 1]);
        let nx = ask("exfil.attacker.example").await;
        assert_eq!(nx[3] & 0x0f, RCODE_NXDOMAIN);
        assert!(std::fs::read_to_string(&log).unwrap().contains("exfil.attacker.example"));

        task.abort();
        let _ = std::fs::remove_file(&log);
    }
}
