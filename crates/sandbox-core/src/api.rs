//! Wire protocol: one JSON `Request` line in, one JSON `Response` line out,
//! per connection.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Directory the workspace is mounted at inside every sandbox.
pub const WORKSPACE_MOUNT: &str = "/workspace";

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    /// Named allowlists available to `--profile`.
    Profiles,
    /// Agent presets available to `--agent`.
    Agents,
    Create(Box<CreateRequest>),
    List,
    /// Look up one sandbox by id, id prefix, name, or a path inside its workspace.
    Get { selector: String },
    Destroy { selector: String, discard: bool },
    /// Add and/or remove allowlist entries on a live sandbox.
    Policy {
        selector: String,
        add: Vec<String>,
        remove: Vec<String>,
    },
    /// Add `[PORT=]HOST:PORT` forwards and/or remove them by gateway port.
    Forward {
        selector: String,
        add: Vec<String>,
        remove: Vec<u16>,
    },
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CreateRequest {
    pub name: Option<String>,
    /// Git repository to cut a treehouse worktree from. `None` means a scratch workspace.
    pub repo: Option<PathBuf>,
    pub base: Option<String>,
    pub branch: Option<String>,
    pub image: Option<String>,
    pub profiles: Vec<String>,
    /// Agent presets from agents/*.toml. Empty means the configured defaults.
    #[serde(default)]
    pub agents: Vec<String>,
    /// Skip the configured default agents.
    #[serde(default)]
    pub no_agents: bool,
    pub allow: Vec<String>,
    pub cpus: Option<u32>,
    pub memory: Option<String>,
    pub ttl_secs: Option<u64>,
    /// `KEY=VALUE` literals stored with the sandbox, or bare `KEY` names the
    /// CLI forwards from its own environment on every exec (never persisted).
    pub env: Vec<String>,
    /// `[PORT=]HOST:PORT` services to relay into the sandbox.
    #[serde(default)]
    pub forward: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok<T: Serialize>(data: &T) -> Self {
        Self {
            ok: true,
            data: Some(serde_json::to_value(data).expect("serializable response")),
            error: None,
        }
    }

    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(message.into()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxState {
    Creating,
    Running,
    /// Container is gone but the workspace was kept (it had uncommitted work).
    Stopped,
    Failed,
    Destroyed,
}

impl std::fmt::Display for SandboxState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Creating => "creating",
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
            Self::Destroyed => "destroyed",
        };
        f.write_str(s)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Workspace {
    /// A treehouse worktree leased for the lifetime of the sandbox.
    Worktree {
        repo: PathBuf,
        path: PathBuf,
        branch: String,
        /// The repository's shared `.git` directory, mounted at the same path
        /// inside the container so the worktree's `gitdir:` pointer resolves.
        git_common_dir: PathBuf,
    },
    /// A throwaway directory under the sandboxd home.
    Scratch { path: PathBuf },
}

impl Workspace {
    pub fn path(&self) -> &PathBuf {
        match self {
            Self::Worktree { path, .. } | Self::Scratch { path } => path,
        }
    }
}

/// An agent preset attached to a sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub name: String,
    pub command: Vec<String>,
}

/// A copy of host agent config, mounted into the sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMount {
    pub agent: String,
    /// The original on the host.
    pub host: PathBuf,
    /// The sandbox's copy (mounted at `container`).
    pub data: PathBuf,
    /// Untouched copies of the `sync_back` files, to detect changes.
    pub seed: PathBuf,
    pub container: PathBuf,
    pub sync_back: Vec<String>,
}

/// A TCP port on the sandbox's gateway that sandboxd relays to a service the
/// host can reach (a database on the Mac or the LAN, say). This is the one
/// way to reach private addresses, so each one is explicit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forward {
    /// Port on the gateway the sandbox connects to.
    pub listen: u16,
    pub host: String,
    pub port: u16,
}

impl Forward {
    /// Parse `[LISTEN=]HOST:PORT`. A missing `LISTEN` comes back as 0, for
    /// sandboxd to pick.
    pub fn parse(spec: &str) -> anyhow::Result<Self> {
        let spec = spec.trim();
        let (listen, target) = match spec.split_once('=') {
            Some((l, t)) => (l.parse().map_err(|_| anyhow::anyhow!("bad port {l:?} in forward {spec:?}"))?, t),
            None => (0, spec),
        };
        let (host, port) = match target.strip_prefix('[') {
            Some(rest) => rest.split_once("]:"),
            None => target.rsplit_once(':').filter(|(h, _)| !h.contains(':')),
        }
        .ok_or_else(|| anyhow::anyhow!("forward {spec:?} should look like [PORT=]HOST:PORT"))?;
        let port: u16 = port.parse().map_err(|_| anyhow::anyhow!("bad port {port:?} in forward {spec:?}"))?;
        if host.is_empty() || port == 0 {
            anyhow::bail!("forward {spec:?} should look like [PORT=]HOST:PORT");
        }
        Ok(Self { listen, host: host.to_string(), port })
    }

    /// The target as `host:port`, bracketing IPv6 literals.
    pub fn target(&self) -> String {
        if self.host.contains(':') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sandbox {
    pub id: String,
    pub name: Option<String>,
    pub state: SandboxState,
    pub container: String,
    pub network: String,
    pub image: String,
    pub workspace: Option<Workspace>,
    #[serde(default)]
    pub agents: Vec<Agent>,
    #[serde(default)]
    pub agent_mounts: Vec<AgentMount>,
    pub allow: Vec<String>,
    #[serde(default)]
    pub forwards: Vec<Forward>,
    pub cpus: u32,
    pub memory: String,
    pub container_ip: Option<String>,
    pub gateway_ip: Option<String>,
    pub proxy_url: Option<String>,
    pub env: Vec<String>,
    pub egress_log: PathBuf,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub error: Option<String>,
}

impl Sandbox {
    /// Environment every process in the sandbox should see: proxy settings
    /// followed by the user's own entries.
    pub fn exec_env(&self) -> Vec<String> {
        let mut env = Vec::new();
        if let Some(proxy) = &self.proxy_url {
            for key in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
                env.push(format!("{key}={proxy}"));
            }
            // The gateway serves forwarded ports; reach those directly.
            let no_proxy = match &self.gateway_ip {
                Some(gw) => format!("localhost,127.0.0.1,::1,{gw}"),
                None => "localhost,127.0.0.1,::1".into(),
            };
            env.push(format!("NO_PROXY={no_proxy}"));
            env.push(format!("no_proxy={no_proxy}"));
            // Node's built-in fetch ignores *_PROXY unless asked (Node >= 24).
            env.push("NODE_USE_ENV_PROXY=1".into());
        }
        env.push(format!("SANDBOX_ID={}", self.id));
        if let Some(gw) = &self.gateway_ip {
            env.push(format!("SANDBOX_GATEWAY={gw}"));
        }
        env.extend(self.env.iter().cloned());
        env
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_forwards() {
        let f = Forward::parse("15432=localhost:5432").unwrap();
        assert_eq!((f.listen, f.host.as_str(), f.port), (15432, "localhost", 5432));
        let f = Forward::parse("10.0.1.20:6379").unwrap();
        assert_eq!((f.listen, f.host.as_str(), f.port), (0, "10.0.1.20", 6379));
        let f = Forward::parse("[fd00::5]:5432").unwrap();
        assert_eq!((f.host.as_str(), f.target()), ("fd00::5", "[fd00::5]:5432".to_string()));
        for bad in ["localhost", "x=localhost:5432", "localhost:0", ":5432", "fd00::5:5432", "localhost:99999"] {
            assert!(Forward::parse(bad).is_err(), "{bad}");
        }
    }
}
