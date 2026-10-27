//! Workspace lifecycle: treehouse worktrees for git repos, scratch dirs otherwise.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sandbox_core::api::Workspace;
use tokio::process::Command;

pub struct Workspaces {
    treehouse: String,
    git: String,
    scratch_root: PathBuf,
}

pub fn lease_holder(id: &str) -> String {
    format!("sandboxd:{id}")
}

impl Workspaces {
    pub fn new(treehouse: String, git: String, scratch_root: PathBuf) -> Self {
        Self { treehouse, git, scratch_root }
    }

    async fn git(&self, dir: &Path, args: &[&str]) -> Result<String> {
        let out = Command::new(&self.git)
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .with_context(|| format!("failed to run `{}`", self.git))?;
        if !out.status.success() {
            bail!("`git {}` failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Lease a fresh treehouse worktree on a new branch.
    pub async fn lease_worktree(
        &self,
        id: &str,
        repo: &Path,
        base: Option<&str>,
        branch: &str,
    ) -> Result<Workspace> {
        let repo = PathBuf::from(self.git(repo, &["rev-parse", "--show-toplevel"]).await.with_context(
            || format!("{} is not inside a git repository", repo.display()),
        )?);
        let holder = lease_holder(id);
        let mut cmd = Command::new(&self.treehouse);
        cmd.current_dir(&repo)
            .args(["get", "--lease", "--lease-holder", &holder, "--branch", branch])
            .stdin(std::process::Stdio::null());
        if let Some(base) = base {
            cmd.args(["--base", base]);
        }
        let out = cmd
            .output()
            .await
            .with_context(|| format!("failed to run `{}`", self.treehouse))?;
        if !out.status.success() {
            bail!("treehouse get failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let path = String::from_utf8_lossy(&out.stdout)
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map(|l| PathBuf::from(l.trim()))
            .context("treehouse get printed no worktree path")?;
        let git_common_dir = PathBuf::from(
            self.git(&path, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).await?,
        );
        Ok(Workspace::Worktree { repo, path, branch: branch.to_string(), git_common_dir })
    }

    pub fn create_scratch(&self, id: &str) -> Result<Workspace> {
        let path = self.scratch_root.join(id);
        std::fs::create_dir_all(&path)?;
        Ok(Workspace::Scratch { path })
    }

    /// Uncommitted changes in a worktree, as `git status --porcelain` lines.
    pub async fn dirty_files(&self, ws: &Workspace) -> Result<Vec<String>> {
        match ws {
            Workspace::Worktree { path, .. } if path.exists() => Ok(self
                .git(path, &["status", "--porcelain"])
                .await?
                .lines()
                .map(str::to_string)
                .collect()),
            _ => Ok(Vec::new()),
        }
    }

    /// Give the workspace back. Worktrees are returned to the treehouse pool
    /// (the sandbox branch and its commits stay in the repo); scratch dirs are
    /// deleted.
    pub async fn release(&self, id: &str, ws: &Workspace) -> Result<()> {
        match ws {
            Workspace::Worktree { path, .. } => {
                if !path.exists() {
                    return Ok(());
                }
                let holder = lease_holder(id);
                let out = Command::new(&self.treehouse)
                    .arg("return")
                    .arg(path)
                    .args(["--force", "--if-lease-holder", &holder])
                    .stdin(std::process::Stdio::null())
                    .output()
                    .await
                    .with_context(|| format!("failed to run `{}`", self.treehouse))?;
                if !out.status.success() {
                    bail!("treehouse return failed: {}", String::from_utf8_lossy(&out.stderr).trim());
                }
                Ok(())
            }
            Workspace::Scratch { path } => {
                if path.starts_with(&self.scratch_root) && path.exists() {
                    std::fs::remove_dir_all(path)?;
                }
                Ok(())
            }
        }
    }
}
