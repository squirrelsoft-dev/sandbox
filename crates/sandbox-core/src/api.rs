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
    Create(CreateRequest),
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
            env.push("NO_PROXY=localhost,127.0.0.1,::1".into());
            env.push("no_proxy=localhost,127.0.0.1,::1".into());
            // Node's built-in fetch ignores *_PROXY unless asked (Node >= 24).
            env.push("NODE_USE_ENV_PROXY=1".into());
        }
        env.push(format!("SANDBOX_ID={}", self.id));
        env.extend(self.env.iter().cloned());
        env
    }
}
