use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Deserialize;

/// `config.toml`. Every field has a default so an empty file is valid.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Image used when `sandbox create` doesn't pass `--image`.
    pub image: String,
    pub cpus: u32,
    pub memory: String,
    /// Sandboxes are torn down after this long unless `--ttl` overrides it.
    /// `"0"` disables the limit.
    #[serde(with = "humantime_serde_compat")]
    pub default_ttl: Duration,
    /// Port the egress proxy listens on, on each sandbox's gateway address.
    pub proxy_port: u16,
    /// Port of the allowlist-scoped DNS responder on each gateway. Port 53
    /// needs root on macOS, so the pf anchor redirects 53 here.
    pub dns_port: u16,
    /// Sandbox networks are carved from these: `<v4>.N.0/24` and `<v6>:N::/64`.
    /// Keeping them fixed lets a static pf anchor fence sandboxes off from the
    /// host (see scripts/firewall.sh).
    pub subnet_prefix_v4: String,
    pub subnet_prefix_v6: String,
    /// Allowlist entries applied to every sandbox.
    pub allow: Vec<String>,
    /// Profiles applied when `--profile` is not given.
    pub default_profiles: Vec<String>,
    /// Agent presets applied when `--agent` is not given.
    pub default_agents: Vec<String>,
    /// Refuse upstream connections that resolve to loopback, private, or
    /// link-local addresses, so an allowlisted name can't be pointed at the
    /// host or the LAN.
    pub block_private_ips: bool,
    /// Path to Apple's `container` CLI.
    pub container_bin: String,
    pub treehouse_bin: String,
    pub git_bin: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            image: "sandbox-agent:latest".into(),
            cpus: 4,
            memory: "4G".into(),
            default_ttl: Duration::from_secs(8 * 3600),
            proxy_port: 3128,
            dns_port: 3053,
            subnet_prefix_v4: "10.213".into(),
            subnet_prefix_v6: "fd5b:d0c5:a17e".into(),
            allow: Vec::new(),
            default_profiles: Vec::new(),
            default_agents: Vec::new(),
            block_private_ips: true,
            container_bin: "container".into(),
            treehouse_bin: "treehouse".into(),
            git_bin: "git".into(),
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            std::fs::write(path, DEFAULT_CONFIG)?;
        }
        let text = std::fs::read_to_string(path)?;
        toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))
    }
}

/// A named allowlist in `policies/<name>.toml`.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    #[serde(default)]
    pub description: String,
    pub allow: Vec<String>,
}

pub fn load_profiles(dir: &Path) -> Result<BTreeMap<String, Profile>> {
    std::fs::create_dir_all(dir)?;
    if std::fs::read_dir(dir)?.next().is_none() {
        for (name, body) in DEFAULT_PROFILES {
            std::fs::write(dir.join(format!("{name}.toml")), body)?;
        }
    }
    let mut out = BTreeMap::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let text = std::fs::read_to_string(&path)?;
        let profile: Profile =
            toml::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
        out.insert(name, profile);
    }
    Ok(out)
}

mod humantime_serde_compat {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer};

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let s = String::deserialize(d)?;
        if s.trim() == "0" {
            return Ok(Duration::ZERO);
        }
        humantime::parse_duration(&s).map_err(serde::de::Error::custom)
    }
}

const DEFAULT_CONFIG: &str = r#"# sandboxd configuration. Restart sandboxd after editing.

# Image for new sandboxes. Build the bundled one with:
#   container build -t sandbox-agent:latest images/agent
image = "sandbox-agent:latest"
cpus = 4
memory = "4G"

# Sandboxes are destroyed after this long ("0" = never). Override with --ttl.
default_ttl = "8h"

# Egress proxy port, bound on each sandbox's private gateway address.
proxy_port = 3128

# DNS for allowlisted names, on each gateway. scripts/firewall.sh redirects
# sandbox queries on port 53 here; without it sandboxes have no DNS.
dns_port = 3053

# Sandbox networks use <v4>.N.0/24 and <v6>:N::/64. scripts/firewall.sh
# installs a pf anchor for these ranges; change both together.
subnet_prefix_v4 = "10.213"
subnet_prefix_v6 = "fd5b:d0c5:a17e"

# Hosts every sandbox may reach, in addition to its profiles and --allow flags.
# Forms: "example.com", "example.com:8443", "*.example.com" (subdomains only),
# ".example.com" (domain and subdomains), "*" (anything; for debugging only).
allow = []

# Profiles from policies/*.toml applied when --profile isn't given.
default_profiles = []

# Agent presets from agents/*.toml applied when --agent isn't given
# (pass --no-agent to skip them), e.g. ["pi"].
default_agents = []

# Refuse upstreams that resolve to loopback/private/link-local addresses.
block_private_ips = true
"#;

const DEFAULT_PROFILES: &[(&str, &str)] = &[
    (
        "claude",
        r#"description = "Claude Code"
allow = ["api.anthropic.com", "statsig.anthropic.com", "claude.ai", "console.anthropic.com"]
"#,
    ),
    (
        "codex",
        r#"description = "OpenAI Codex CLI"
allow = ["api.openai.com", "chatgpt.com", "auth.openai.com"]
"#,
    ),
    (
        "github",
        r#"description = "GitHub over HTTPS (git, API, release downloads)"
allow = [
  "github.com",
  "api.github.com",
  "codeload.github.com",
  "objects.githubusercontent.com",
  "raw.githubusercontent.com",
  "release-assets.githubusercontent.com",
]
"#,
    ),
    (
        "npm",
        r#"description = "npm registry"
allow = ["registry.npmjs.org"]
"#,
    ),
    (
        "pypi",
        r#"description = "Python Package Index"
allow = ["pypi.org", "files.pythonhosted.org"]
"#,
    ),
    (
        "crates",
        r#"description = "crates.io"
allow = ["crates.io", "index.crates.io", "static.crates.io", "static.rust-lang.org"]
"#,
    ),
];
