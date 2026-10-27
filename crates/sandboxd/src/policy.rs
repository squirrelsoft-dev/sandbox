//! Egress allowlist matching.

use anyhow::{bail, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
enum HostPattern {
    Any,
    Exact(String),
    /// `*.example.com`: strict subdomains only.
    Subdomains(String),
    /// `.example.com`: the domain itself and its subdomains.
    DomainAndSubdomains(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    host: HostPattern,
    /// `None` means the default web ports, 80 and 443.
    port: Option<u16>,
}

impl Rule {
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim().to_ascii_lowercase();
        if raw.is_empty() {
            bail!("empty allowlist entry");
        }
        if raw.contains("://") || raw.contains('/') {
            bail!("allowlist entry {raw:?} should be a host, not a URL");
        }
        let (host, port) = split_host_port(&raw)?;
        let host = host.trim_end_matches('.');
        let host = if host == "*" {
            HostPattern::Any
        } else if let Some(rest) = host.strip_prefix("*.") {
            HostPattern::Subdomains(rest.to_string())
        } else if let Some(rest) = host.strip_prefix('.') {
            HostPattern::DomainAndSubdomains(rest.to_string())
        } else if host.contains('*') {
            bail!("wildcards are only supported as a leading `*.` in {raw:?}");
        } else {
            HostPattern::Exact(host.to_string())
        };
        Ok(Self { host, port })
    }

    fn matches(&self, host: &str, port: u16) -> bool {
        let port_ok = match self.port {
            Some(p) => p == port,
            None => port == 80 || port == 443,
        };
        port_ok && self.matches_host(host)
    }

    fn matches_host(&self, host: &str) -> bool {
        match &self.host {
            HostPattern::Any => true,
            HostPattern::Exact(h) => host == h,
            HostPattern::Subdomains(d) => is_subdomain(host, d),
            HostPattern::DomainAndSubdomains(d) => host == d || is_subdomain(host, d),
        }
    }
}

fn is_subdomain(host: &str, domain: &str) -> bool {
    host.len() > domain.len() + 1
        && host.ends_with(domain)
        && host.as_bytes()[host.len() - domain.len() - 1] == b'.'
}

/// Split `host[:port]`, allowing bracketed IPv6 literals.
pub fn split_host_port(raw: &str) -> Result<(&str, Option<u16>)> {
    if let Some(rest) = raw.strip_prefix('[') {
        let Some((host, tail)) = rest.split_once(']') else {
            bail!("unterminated IPv6 literal in {raw:?}");
        };
        return match tail.strip_prefix(':') {
            Some(p) => Ok((host, Some(p.parse()?))),
            None if tail.is_empty() => Ok((host, None)),
            None => bail!("invalid host {raw:?}"),
        };
    }
    match raw.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => Ok((host, Some(port.parse()?))),
        _ => Ok((raw, None)),
    }
}

#[derive(Debug, Clone, Default)]
pub struct Policy {
    rules: Vec<Rule>,
}

impl Policy {
    pub fn new(entries: &[String]) -> Result<Self> {
        let rules = entries.iter().map(|e| Rule::parse(e)).collect::<Result<_>>()?;
        Ok(Self { rules })
    }

    pub fn allows(&self, host: &str, port: u16) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.rules.iter().any(|r| r.matches(&host, port))
    }

    /// Whether `host` is allowed on any port. Used to decide which names the
    /// sandbox may resolve.
    pub fn allows_name(&self, host: &str) -> bool {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        self.rules.iter().any(|r| r.matches_host(&host))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(entries: &[&str]) -> Policy {
        Policy::new(&entries.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn exact_host_default_ports() {
        let p = policy(&["api.github.com"]);
        assert!(p.allows("api.github.com", 443));
        assert!(p.allows("API.GitHub.com.", 80));
        assert!(!p.allows("api.github.com", 22));
        assert!(!p.allows("github.com", 443));
        assert!(!p.allows("evil-api.github.com", 443));
    }

    #[test]
    fn explicit_port() {
        let p = policy(&["example.com:8443"]);
        assert!(p.allows("example.com", 8443));
        assert!(!p.allows("example.com", 443));
    }

    #[test]
    fn wildcards() {
        let p = policy(&["*.githubusercontent.com"]);
        assert!(p.allows("raw.githubusercontent.com", 443));
        assert!(!p.allows("githubusercontent.com", 443));
        assert!(!p.allows("evilgithubusercontent.com", 443));

        let p = policy(&[".npmjs.org"]);
        assert!(p.allows("npmjs.org", 443));
        assert!(p.allows("registry.npmjs.org", 443));
        assert!(!p.allows("xnpmjs.org", 443));
    }

    #[test]
    fn any() {
        assert!(policy(&["*"]).allows("anything.example", 443));
        assert!(policy(&["*:22"]).allows("anything.example", 22));
    }

    #[test]
    fn rejects_bad_entries() {
        for bad in ["https://github.com", "github.com/foo", "a*.b.com", ""] {
            assert!(Rule::parse(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn ipv6() {
        assert_eq!(split_host_port("[::1]:443").unwrap(), ("::1", Some(443)));
        assert_eq!(split_host_port("[::1]").unwrap(), ("::1", None));
    }
}
