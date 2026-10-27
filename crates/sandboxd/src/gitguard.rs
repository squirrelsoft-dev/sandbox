//! Guards the host against code execution planted through the shared `.git`.
//!
//! A worktree only works inside the container if the repository's common
//! `.git` directory is mounted too, and it has to be writable for commits.
//! That would let an agent drop a hook or a config entry (`core.fsmonitor`,
//! `core.hooksPath`, a filter driver, ...) that runs on the host the next time
//! you use git there. `hooks/` is mounted read-only; Apple's runtime cannot
//! bind-mount single files, so `config` is instead watched: any newly added
//! execution-capable entry is stripped and restored to its snapshotted value.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tokio::process::Command;

type Entries = BTreeSet<(String, String)>;

pub struct GitGuard {
    git: String,
    snapshots: PathBuf,
}

impl GitGuard {
    pub fn new(git: String, snapshots: PathBuf) -> Self {
        Self { git, snapshots }
    }

    fn snapshot_path(&self, id: &str) -> PathBuf {
        self.snapshots.join(format!("{id}.json"))
    }

    async fn dangerous_entries(&self, config: &Path) -> Result<Entries> {
        let out = Command::new(&self.git)
            .args(["config", "--file"])
            .arg(config)
            .args(["--list", "--null"])
            .output()
            .await
            .context("run git config")?;
        if !out.status.success() {
            bail!("git config --list failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(text
            .split('\0')
            .filter_map(|e| e.split_once('\n'))
            .filter(|(k, v)| is_dangerous(&k.to_ascii_lowercase(), v))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect())
    }

    /// Record the execution-capable entries present before the sandbox starts.
    pub async fn snapshot(&self, id: &str, git_common_dir: &Path) -> Result<()> {
        std::fs::create_dir_all(&self.snapshots)?;
        let entries = self.dangerous_entries(&git_common_dir.join("config")).await?;
        std::fs::write(self.snapshot_path(id), serde_json::to_vec(&entries)?)?;
        Ok(())
    }

    /// Strip entries added since the snapshot. Returns the keys that were reverted.
    pub async fn enforce(&self, id: &str, git_common_dir: &Path) -> Result<Vec<String>> {
        let config = git_common_dir.join("config");
        let snapshot: Entries = match std::fs::read(self.snapshot_path(id)) {
            Ok(bytes) => serde_json::from_slice(&bytes)?,
            Err(_) => return Ok(Vec::new()),
        };
        let current = self.dangerous_entries(&config).await?;
        let keys: BTreeSet<&String> = current.difference(&snapshot).map(|(k, _)| k).collect();
        for key in &keys {
            self.config(&config, &["--unset-all", key]).await?;
            for (_, value) in snapshot.iter().filter(|(k, _)| k == *key) {
                self.config(&config, &["--add", key, value]).await?;
            }
        }
        Ok(keys.into_iter().cloned().collect())
    }

    async fn config(&self, file: &Path, args: &[&str]) -> Result<()> {
        let status = Command::new(&self.git)
            .args(["config", "--file"])
            .arg(file)
            .args(args)
            .status()
            .await?;
        if !status.success() {
            bail!("git config {} failed", args.join(" "));
        }
        Ok(())
    }

    pub fn forget(&self, id: &str) {
        let _ = std::fs::remove_file(self.snapshot_path(id));
    }
}

/// Config keys that can make git run an arbitrary program.
fn is_dangerous(key: &str, value: &str) -> bool {
    const EXACT: &[&str] = &[
        "core.hookspath",
        "core.fsmonitor",
        "core.sshcommand",
        "core.pager",
        "core.editor",
        "core.askpass",
        "core.gitproxy",
        "core.worktree",
        "sequence.editor",
        "gpg.program",
        "credential.helper",
        "diff.external",
        "include.path",
        "extensions.worktreeconfig",
        "interactive.difffilter",
        "uploadpack.packobjectshook",
        "web.browser",
    ];
    // (prefix, suffix) for `section.<subsection>.key` forms.
    const PATTERNS: &[(&str, &str)] = &[
        ("includeif.", ".path"),
        ("credential.", ".helper"),
        ("gpg.", ".program"),
        ("filter.", ".clean"),
        ("filter.", ".smudge"),
        ("filter.", ".process"),
        ("diff.", ".textconv"),
        ("diff.", ".command"),
        ("merge.", ".driver"),
        ("difftool.", ".cmd"),
        ("mergetool.", ".cmd"),
        ("browser.", ".cmd"),
        ("man.", ".cmd"),
        ("remote.", ".uploadpack"),
        ("remote.", ".receivepack"),
        ("protocol.", ".allow"),
    ];
    if EXACT.contains(&key) || key.starts_with("pager.") {
        return true;
    }
    if PATTERNS.iter().any(|(p, s)| key.starts_with(p) && key.ends_with(s)) {
        return true;
    }
    let shell_value = value.trim_start().starts_with('!');
    (key.starts_with("alias.") && shell_value)
        || (key.starts_with("submodule.") && key.ends_with(".update") && shell_value)
        || (key.starts_with("remote.") && key.ends_with(".url") && value.starts_with("ext::"))
}

#[cfg(test)]
mod tests {
    use super::is_dangerous;

    #[test]
    fn flags_execution_keys() {
        assert!(is_dangerous("core.fsmonitor", "evil.sh"));
        assert!(is_dangerous("filter.lfs.smudge", "git-lfs smudge"));
        assert!(is_dangerous("includeif.gitdir:~/x.path", "/tmp/x"));
        assert!(is_dangerous("alias.st", "!sh -c evil"));
        assert!(is_dangerous("remote.origin.url", "ext::sh -c evil"));
        assert!(is_dangerous("pager.log", "less"));
    }

    #[test]
    fn ignores_benign_keys() {
        assert!(!is_dangerous("alias.st", "status"));
        assert!(!is_dangerous("remote.origin.url", "https://github.com/x/y"));
        assert!(!is_dangerous("branch.main.remote", "origin"));
        assert!(!is_dangerous("user.email", "a@b.c"));
    }
}
