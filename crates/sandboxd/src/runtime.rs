//! Thin async wrapper over Apple's `container` CLI.

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use tokio::process::Command;

/// Label on every container and network sandboxd creates, so orphans can be
/// found and reclaimed after a crash.
pub const MANAGED_LABEL: &str = "dev.squirrelsoft.sandboxd";
pub const ID_LABEL: &str = "dev.squirrelsoft.sandboxd.id";

pub struct Runtime {
    bin: String,
}

pub struct Mount {
    pub source: PathBuf,
    pub target: PathBuf,
    pub readonly: bool,
}

pub struct RunSpec<'a> {
    pub name: &'a str,
    pub id: &'a str,
    pub network: &'a str,
    pub image: &'a str,
    pub cpus: u32,
    pub memory: &'a str,
    pub workdir: &'a str,
    pub mounts: Vec<Mount>,
    pub env: Vec<String>,
}

#[derive(Debug)]
pub struct ContainerStatus {
    pub running: bool,
    pub ip: Option<IpAddr>,
    pub gateway: Option<IpAddr>,
}

// The JSON shapes below accept both container 0.x and 1.x output. 1.0
// moved a container's state and network attachments under `status`, and a
// network's `config` to `configuration`.

#[derive(Deserialize)]
struct InspectContainer {
    status: InspectContainerStatus,
    /// 0.x: attachments at the top level.
    #[serde(default)]
    networks: Vec<InspectAttachment>,
    configuration: InspectConfig,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum InspectContainerStatus {
    /// 0.x: `"status": "running"`.
    State(String),
    /// 1.x: `"status": {"state": "running", "networks": [...]}`.
    Detailed {
        state: String,
        #[serde(default)]
        networks: Vec<InspectAttachment>,
    },
}

impl InspectContainer {
    fn state(&self) -> &str {
        match &self.status {
            InspectContainerStatus::State(s) | InspectContainerStatus::Detailed { state: s, .. } => s,
        }
    }

    fn attachment(&self) -> Option<&InspectAttachment> {
        match &self.status {
            InspectContainerStatus::Detailed { networks, .. } if !networks.is_empty() => networks.first(),
            _ => self.networks.first(),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InspectAttachment {
    ipv4_address: Option<String>,
    ipv4_gateway: Option<String>,
}

#[derive(Deserialize)]
struct InspectConfig {
    id: String,
    #[serde(default)]
    labels: std::collections::HashMap<String, String>,
}

#[derive(Deserialize)]
struct InspectNetwork {
    id: String,
    #[serde(default, alias = "configuration")]
    config: InspectNetworkConfig,
    #[serde(default)]
    status: InspectNetworkStatus,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct InspectNetworkStatus {
    ipv4_subnet: Option<String>,
}

#[derive(Deserialize, Default)]
struct InspectNetworkConfig {
    #[serde(default)]
    labels: std::collections::HashMap<String, String>,
}

impl Runtime {
    pub fn new(bin: String) -> Self {
        Self { bin }
    }

    async fn run(&self, args: &[&str]) -> Result<String> {
        tracing::debug!(cmd = %format!("{} {}", self.bin, args.join(" ")), "exec");
        let out = Command::new(&self.bin)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .with_context(|| format!("failed to run `{}`", self.bin))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!("`container {}` failed: {}", args.join(" "), stderr.trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    pub async fn check(&self) -> Result<()> {
        self.run(&["system", "status"])
            .await
            .map(drop)
            .context("Apple container services are not running; start them with `container system start`")
    }

    pub async fn create_network(&self, name: &str, id: &str, subnet: &str, subnet_v6: &str) -> Result<()> {
        let managed = format!("{MANAGED_LABEL}=1");
        let id_label = format!("{ID_LABEL}={id}");
        self.run(&[
            "network", "create", "--internal", "--label", &managed, "--label", &id_label,
            "--subnet", subnet, "--subnet-v6", subnet_v6, name,
        ])
        .await
        .map(drop)
    }

    /// IPv4 subnets of every network the runtime knows about.
    pub async fn used_subnets(&self) -> Result<Vec<String>> {
        let out = self.run(&["network", "list", "--format", "json"]).await?;
        let list: Vec<InspectNetwork> = serde_json::from_str(&out)?;
        Ok(list.into_iter().filter_map(|n| n.status.ipv4_subnet).collect())
    }

    /// Delete a network, retrying while the runtime releases its IP allocations.
    pub async fn delete_network(&self, name: &str) -> Result<()> {
        let mut last = None;
        for _ in 0..10 {
            match self.run(&["network", "delete", name]).await {
                Ok(_) => return Ok(()),
                Err(e) if e.to_string().contains("not found") => return Ok(()),
                Err(e) => last = Some(e),
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        Err(last.unwrap())
    }

    pub async fn run_container(&self, spec: &RunSpec<'_>) -> Result<()> {
        let mut args: Vec<String> = vec![
            "run".into(),
            "--detach".into(),
            // A real PID 1 so `container stop` delivers SIGTERM promptly and
            // agent subprocesses get reaped.
            "--init".into(),
            "--name".into(),
            spec.name.into(),
            "--network".into(),
            spec.network.into(),
            "--label".into(),
            format!("{MANAGED_LABEL}=1"),
            "--label".into(),
            format!("{ID_LABEL}={}", spec.id),
            "--cpus".into(),
            spec.cpus.to_string(),
            "--memory".into(),
            spec.memory.into(),
            "--workdir".into(),
            spec.workdir.into(),
            "--progress".into(),
            "none".into(),
        ];
        for m in &spec.mounts {
            let mut mount = format!(
                "type=bind,source={},target={}",
                m.source.display(),
                m.target.display()
            );
            if m.readonly {
                mount.push_str(",readonly");
            }
            args.push("--mount".into());
            args.push(mount);
        }
        for e in &spec.env {
            args.push("--env".into());
            args.push(e.clone());
        }
        args.extend(["--entrypoint".into(), "sleep".into(), spec.image.into(), "infinity".into()]);
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        self.run(&args).await.map(drop)
    }

    pub async fn inspect(&self, name: &str) -> Result<Option<ContainerStatus>> {
        let out = match self.run(&["inspect", name]).await {
            Ok(out) => out,
            Err(e) if e.to_string().contains("not found") => return Ok(None),
            Err(e) => return Err(e),
        };
        let mut list: Vec<InspectContainer> = serde_json::from_str(&out)?;
        let Some(c) = list.pop() else { return Ok(None) };
        let net = c.attachment();
        let parse = |s: Option<&String>| -> Option<IpAddr> {
            s.and_then(|s| s.split('/').next()).and_then(|s| s.parse().ok())
        };
        Ok(Some(ContainerStatus {
            running: c.state() == "running",
            ip: parse(net.and_then(|n| n.ipv4_address.as_ref())),
            gateway: parse(net.and_then(|n| n.ipv4_gateway.as_ref())),
        }))
    }

    pub async fn stop_and_delete(&self, name: &str) -> Result<()> {
        if self.inspect(name).await?.is_none() {
            return Ok(());
        }
        // Stopping first matters: force-deleting a running container leaves
        // its IP allocated and the network undeletable until the runtime restarts.
        if let Err(e) = self.run(&["stop", name]).await {
            tracing::warn!(container = name, error = %e, "stop failed, forcing delete");
            self.run(&["delete", "--force", name]).await?;
            return Ok(());
        }
        self.run(&["delete", name]).await.map(drop)
    }

    pub async fn exec(&self, name: &str, env: &[String], script: &str) -> Result<String> {
        let mut args = vec!["exec"];
        for e in env {
            args.extend(["--env", e.as_str()]);
        }
        args.extend(["--workdir", sandbox_core::api::WORKSPACE_MOUNT, name, "/bin/sh", "-c", script]);
        self.run(&args).await
    }

    /// `(container name, sandbox id)` for every container carrying our label.
    pub async fn managed_containers(&self) -> Result<Vec<(String, String)>> {
        let out = self.run(&["list", "--all", "--format", "json"]).await?;
        let list: Vec<InspectContainer> = serde_json::from_str(&out)
            .map_err(|e| anyhow!("cannot parse `container list` output: {e}"))?;
        Ok(list
            .into_iter()
            .filter(|c| c.configuration.labels.contains_key(MANAGED_LABEL))
            .map(|c| {
                let id = c.configuration.labels.get(ID_LABEL).cloned().unwrap_or_default();
                (c.configuration.id, id)
            })
            .collect())
    }

    /// `(network name, sandbox id)` for every network carrying our label.
    pub async fn managed_networks(&self) -> Result<Vec<(String, String)>> {
        let out = self.run(&["network", "list", "--format", "json"]).await?;
        let list: Vec<InspectNetwork> = serde_json::from_str(&out)
            .map_err(|e| anyhow!("cannot parse `container network list` output: {e}"))?;
        Ok(list
            .into_iter()
            .filter(|n| n.config.labels.contains_key(MANAGED_LABEL))
            .map(|n| {
                let id = n.config.labels.get(ID_LABEL).cloned().unwrap_or_default();
                (n.id, id)
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_container_json_from_both_major_versions() {
        let v0 = r#"[{"status":"running","networks":[{"ipv4Address":"10.213.1.2/24","ipv4Gateway":"10.213.1.1"}],
            "configuration":{"id":"sbx-a","labels":{"dev.squirrelsoft.sandboxd":"1"}}}]"#;
        let v1 = r#"[{"id":"sbx-a","status":{"state":"running","networks":[{"ipv4Address":"10.213.1.2/24",
            "ipv4Gateway":"10.213.1.1","network":"sbx-a-net"}],"startedDate":"2026-10-27T17:02:45Z"},
            "configuration":{"id":"sbx-a","labels":{"dev.squirrelsoft.sandboxd":"1"},"networks":[{"network":"sbx-a-net"}]}}]"#;
        for json in [v0, v1] {
            let c: Vec<InspectContainer> = serde_json::from_str(json).unwrap();
            assert_eq!(c[0].state(), "running");
            assert_eq!(c[0].attachment().unwrap().ipv4_gateway.as_deref(), Some("10.213.1.1"));
            assert!(c[0].configuration.labels.contains_key(MANAGED_LABEL));
        }
    }

    #[test]
    fn parses_network_json_from_both_major_versions() {
        let v0 = r#"[{"id":"n","config":{"labels":{"dev.squirrelsoft.sandboxd":"1"}},"status":{"ipv4Subnet":"10.213.1.0/24"}}]"#;
        let v1 = r#"[{"id":"n","configuration":{"labels":{"dev.squirrelsoft.sandboxd":"1"},"mode":"hostOnly"},
            "status":{"ipv4Gateway":"10.213.1.1","ipv4Subnet":"10.213.1.0/24"}}]"#;
        for json in [v0, v1] {
            let n: Vec<InspectNetwork> = serde_json::from_str(json).unwrap();
            assert!(n[0].config.labels.contains_key(MANAGED_LABEL));
            assert_eq!(n[0].status.ipv4_subnet.as_deref(), Some("10.213.1.0/24"));
        }
    }
}
