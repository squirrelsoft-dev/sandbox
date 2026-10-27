//! Sandbox lifecycle: create, destroy, policy updates, crash recovery, expiry.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::net::IpAddr;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use chrono::Utc;
use sandbox_core::api::{Agent, CreateRequest, Forward, Sandbox, SandboxState, Workspace, WORKSPACE_MOUNT};
use sandbox_core::paths::Paths;

use crate::agents::{self, Preset};
use crate::config::{Config, Profile};
use crate::gitguard::GitGuard;
use crate::policy::Policy;
use crate::proxy::{self, ProxyHandle};
use crate::runtime::{Mount, RunSpec, Runtime};
use crate::state::Store;
use crate::workspace::Workspaces;

pub struct Manager {
    cfg: Config,
    profiles: BTreeMap<String, Profile>,
    presets: BTreeMap<String, Preset>,
    paths: Paths,
    store: Store,
    rt: Runtime,
    ws: Workspaces,
    guard: GitGuard,
    proxies: Mutex<HashMap<String, ProxyHandle>>,
    busy: Mutex<HashSet<String>>,
    /// Serializes subnet allocation with network creation.
    net_alloc: tokio::sync::Mutex<()>,
}

/// Marks a sandbox as having an operation in flight until dropped.
struct BusyGuard<'a> {
    set: &'a Mutex<HashSet<String>>,
    id: String,
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.set.lock().unwrap().remove(&self.id);
    }
}

/// What to do with a worktree that still has uncommitted changes.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dirty {
    /// Refuse to destroy anything (explicit `sandbox destroy`).
    Refuse,
    /// Tear down the container but keep the workspace (expiry, crash recovery).
    Keep,
    /// Throw the changes away (`--discard`).
    Discard,
}

impl Manager {
    pub fn new(
        cfg: Config,
        profiles: BTreeMap<String, Profile>,
        presets: BTreeMap<String, Preset>,
        paths: Paths,
        store: Store,
    ) -> Self {
        Self {
            rt: Runtime::new(cfg.container_bin.clone()),
            ws: Workspaces::new(cfg.treehouse_bin.clone(), cfg.git_bin.clone(), paths.workspaces()),
            guard: GitGuard::new(cfg.git_bin.clone(), paths.home.join("git-snapshots")),
            cfg,
            profiles,
            presets,
            paths,
            store,
            proxies: Mutex::new(HashMap::new()),
            busy: Mutex::new(HashSet::new()),
            net_alloc: tokio::sync::Mutex::new(()),
        }
    }

    fn claim(&self, id: &str) -> Result<BusyGuard<'_>> {
        if !self.busy.lock().unwrap().insert(id.to_string()) {
            bail!("sandbox {id} is busy with another operation");
        }
        Ok(BusyGuard { set: &self.busy, id: id.to_string() })
    }

    pub fn profiles(&self) -> &BTreeMap<String, Profile> {
        &self.profiles
    }

    pub fn presets(&self) -> &BTreeMap<String, Preset> {
        &self.presets
    }

    pub fn list(&self) -> Result<Vec<Sandbox>> {
        self.store.live()
    }

    /// Find a live sandbox by id, name, unique id prefix, or a path inside its
    /// workspace. An empty selector matches the only live sandbox, if there is one.
    pub fn resolve(&self, selector: &str) -> Result<Sandbox> {
        let live = self.store.live()?;
        if selector.is_empty() {
            return match live.len() {
                1 => Ok(live.into_iter().next().unwrap()),
                0 => bail!("no sandboxes are running"),
                n => bail!("{n} sandboxes exist; say which one"),
            };
        }
        if let Some(sb) = live.iter().find(|s| s.id == selector || s.name.as_deref() == Some(selector)) {
            return Ok(sb.clone());
        }
        let path = Path::new(selector);
        if path.is_absolute() {
            let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
            if let Some(sb) = live.iter().find(|s| {
                s.workspace.as_ref().is_some_and(|w| {
                    let root = w.path().canonicalize().unwrap_or_else(|_| w.path().clone());
                    path.starts_with(root)
                })
            }) {
                return Ok(sb.clone());
            }
            bail!("no sandbox owns {selector}");
        }
        let matches: Vec<&Sandbox> = live.iter().filter(|s| s.id.starts_with(selector)).collect();
        match matches.as_slice() {
            [one] => Ok((*one).clone()),
            [] => bail!("no sandbox matches {selector:?}"),
            _ => bail!("{selector:?} is ambiguous"),
        }
    }

    fn allowlist(&self, names: &[String], extra: &[String]) -> Result<Vec<String>> {
        let mut out: BTreeSet<String> = self.cfg.allow.iter().cloned().collect();
        for name in names {
            let profile = self.profiles.get(name).ok_or_else(|| {
                let known: Vec<&str> = self.profiles.keys().map(String::as_str).collect();
                anyhow!("unknown profile {name:?} (known: {})", known.join(", "))
            })?;
            out.extend(profile.allow.iter().cloned());
        }
        out.extend(extra.iter().map(|s| s.trim().to_string()));
        let out: Vec<String> = out.into_iter().collect();
        Policy::new(&out)?;
        Ok(out)
    }

    pub async fn create(&self, req: CreateRequest) -> Result<Sandbox> {
        let agent_names = match (req.no_agents, req.agents.is_empty()) {
            (true, _) => Vec::new(),
            (false, true) => self.cfg.default_agents.clone(),
            (false, false) => req.agents.clone(),
        };
        let mut presets = Vec::new();
        for name in &agent_names {
            let preset = self.presets.get(name).ok_or_else(|| {
                let known: Vec<&str> = self.presets.keys().map(String::as_str).collect();
                anyhow!("unknown agent {name:?} (known: {})", known.join(", "))
            })?;
            presets.push((name.clone(), preset.clone()));
        }
        let mut profiles = if req.profiles.is_empty() { self.cfg.default_profiles.clone() } else { req.profiles.clone() };
        let mut extra_allow = req.allow.clone();
        let mut env = Vec::new();
        let mut forward_specs = Vec::new();
        for (_, p) in &presets {
            profiles.extend(p.profiles.iter().cloned());
            extra_allow.extend(p.allow.iter().cloned());
            env.extend(p.env.iter().cloned());
            forward_specs.extend(p.forward.iter().cloned());
        }
        forward_specs.extend(req.forward.iter().cloned());
        let forwards = self.assign_forwards(&[], &forward_specs)?;
        for e in &req.env {
            if !env.contains(e) {
                env.push(e.clone());
            }
        }
        let allow = self.allowlist(&profiles, &extra_allow)?;
        let id = loop {
            let id = format!("{:06x}", rand::random::<u32>() & 0xff_ffff);
            if self.store.get(&id)?.is_none() {
                break id;
            }
        };
        let _busy = self.claim(&id)?;
        if let Some(name) = &req.name {
            if self.store.live()?.iter().any(|s| s.name.as_deref() == Some(name)) {
                bail!("a sandbox named {name:?} already exists");
            }
        }

        let ttl = req.ttl_secs.map(Duration::from_secs).unwrap_or(self.cfg.default_ttl);
        let mut sb = Sandbox {
            id: id.clone(),
            name: req.name.clone(),
            state: SandboxState::Creating,
            container: format!("sbx-{id}"),
            network: format!("sbx-{id}-net"),
            image: req.image.clone().unwrap_or_else(|| self.cfg.image.clone()),
            workspace: None,
            agents: presets
                .iter()
                .map(|(name, p)| Agent { name: name.clone(), command: p.command.clone() })
                .collect(),
            agent_mounts: Vec::new(),
            allow,
            forwards,
            cpus: req.cpus.unwrap_or(self.cfg.cpus),
            memory: req.memory.clone().unwrap_or_else(|| self.cfg.memory.clone()),
            container_ip: None,
            gateway_ip: None,
            proxy_url: None,
            env,
            egress_log: self.paths.egress_log(&id),
            created_at: Utc::now(),
            expires_at: (!ttl.is_zero())
                .then(|| Utc::now() + chrono::Duration::from_std(ttl).unwrap_or(chrono::Duration::MAX)),
            error: None,
        };
        self.store.put(&sb)?;
        self.store.event(&id, "create", &serde_json::to_string(&req)?);

        match self.provision(&mut sb, &req, &presets).await {
            Ok(()) => {
                sb.state = SandboxState::Running;
                self.store.put(&sb)?;
                tracing::info!(id = %sb.id, "sandbox running");
                Ok(sb)
            }
            Err(e) => {
                tracing::error!(id = %sb.id, error = %format!("{e:#}"), "create failed, cleaning up");
                sb.error = Some(format!("{e:#}"));
                // Nothing has run in the workspace yet, so it is safe to discard.
                if let Err(ce) = self.teardown(&mut sb, Dirty::Discard).await {
                    tracing::error!(id = %sb.id, error = %format!("{ce:#}"), "cleanup after failed create");
                    sb.state = SandboxState::Failed;
                    self.store.put(&sb)?;
                }
                Err(e)
            }
        }
    }

    async fn provision(&self, sb: &mut Sandbox, req: &CreateRequest, presets: &[(String, Preset)]) -> Result<()> {
        self.rt.check().await?;

        let workspace = match &req.repo {
            Some(repo) => {
                let branch = req.branch.clone().unwrap_or_else(|| format!("sandbox/{}", sb.id));
                self.ws.lease_worktree(&sb.id, repo, req.base.as_deref(), &branch).await?
            }
            None => self.ws.create_scratch(&sb.id)?,
        };
        sb.workspace = Some(workspace.clone());
        self.store.put(sb)?;

        let mut mounts = vec![Mount {
            source: workspace.path().clone(),
            target: WORKSPACE_MOUNT.into(),
            readonly: false,
        }];
        if let Workspace::Worktree { git_common_dir, .. } = &workspace {
            self.guard.snapshot(&sb.id, git_common_dir).await?;
            let hooks = git_common_dir.join("hooks");
            std::fs::create_dir_all(&hooks)?;
            mounts.push(Mount { source: git_common_dir.clone(), target: git_common_dir.clone(), readonly: false });
            mounts.push(Mount { source: hooks.clone(), target: hooks, readonly: true });
        }

        let state_dir = self.agent_state_dir(&sb.id);
        for (name, preset) in presets {
            let seeded = agents::seed(name, preset, &state_dir)?;
            sb.agent_mounts.extend(seeded);
        }
        self.store.put(sb)?;
        for m in &sb.agent_mounts {
            mounts.push(Mount { source: m.data.clone(), target: m.container.clone(), readonly: false });
        }

        self.create_network(sb).await?;

        // The proxy URL has to be known before the container starts so it can
        // go in the init environment, but the gateway is only assigned once
        // the network exists. Start the container, then read both addresses.
        let spec = RunSpec {
            name: &sb.container,
            id: &sb.id,
            network: &sb.network,
            image: &sb.image,
            cpus: sb.cpus,
            memory: &sb.memory,
            workdir: WORKSPACE_MOUNT,
            mounts,
            env: sb.env.iter().filter(|e| e.contains('=')).cloned().collect(),
        };
        self.rt.run_container(&spec).await?;
        let status = self
            .rt
            .inspect(&sb.container)
            .await?
            .ok_or_else(|| anyhow!("container {} vanished after start", sb.container))?;
        let (Some(ip), Some(gateway)) = (status.ip, status.gateway) else {
            bail!("container {} has no IPv4 address", sb.container);
        };
        sb.container_ip = Some(ip.to_string());
        sb.gateway_ip = Some(gateway.to_string());
        sb.proxy_url = Some(format!("http://{gateway}:{}", self.cfg.proxy_port));
        self.store.put(sb)?;

        self.start_proxy(sb, gateway, ip).await?;
        self.write_profile(sb).await;
        self.run_setup(sb, presets).await
    }

    /// Run preset setup commands with their extra hosts temporarily allowed.
    async fn run_setup(&self, sb: &Sandbox, presets: &[(String, Preset)]) -> Result<()> {
        let steps: Vec<(&str, &String)> = presets
            .iter()
            .flat_map(|(name, p)| p.setup.iter().map(move |cmd| (name.as_str(), cmd)))
            .collect();
        if steps.is_empty() {
            return Ok(());
        }
        let mut allow = sb.allow.clone();
        allow.extend(presets.iter().flat_map(|(_, p)| p.setup_allow.iter().cloned()));
        self.set_proxy_policy(&sb.id, Policy::new(&allow)?);
        let env: Vec<String> = sb.exec_env().into_iter().filter(|e| e.contains('=')).collect();
        let mut result = Ok(());
        for (agent, cmd) in steps {
            tracing::info!(id = %sb.id, agent, "running agent setup");
            if let Err(e) = self.rt.exec(&sb.container, &env, cmd).await {
                result = Err(e.context(format!("{agent} setup failed")));
                break;
            }
        }
        self.set_proxy_policy(&sb.id, Policy::new(&sb.allow)?);
        result
    }

    fn set_proxy_policy(&self, id: &str, policy: Policy) {
        if let Some(handle) = self.proxies.lock().unwrap().get(id) {
            *handle.policy.write().unwrap() = policy;
        }
    }

    /// Create the sandbox's host-only network on the first free slot in the pool.
    async fn create_network(&self, sb: &Sandbox) -> Result<()> {
        let _lock = self.net_alloc.lock().await;
        let used = self.rt.used_subnets().await?;
        let v4 = &self.cfg.subnet_prefix_v4;
        let slot = (1..=254u32)
            .find(|n| !used.contains(&format!("{v4}.{n}.0/24")))
            .ok_or_else(|| anyhow!("no free subnets left in {v4}.0.0/16"))?;
        let subnet = format!("{v4}.{slot}.0/24");
        let subnet_v6 = format!("{}:{slot:x}::/64", self.cfg.subnet_prefix_v6);
        self.rt.create_network(&sb.network, &sb.id, &subnet, &subnet_v6).await
    }

    async fn start_proxy(&self, sb: &Sandbox, gateway: IpAddr, ip: IpAddr) -> Result<()> {
        let handle = proxy::spawn(
            gateway,
            self.cfg.proxy_port,
            self.cfg.dns_port,
            ip,
            Policy::new(&sb.allow)?,
            self.cfg.block_private_ips,
            sb.egress_log.clone(),
        )
        .await?;
        let (gateway, ctx) = handle.forward_context();
        for f in &sb.forwards {
            handle.attach_forward(f.listen, crate::forward::spawn(gateway, f.clone(), ctx.clone()).await?);
        }
        self.proxies.lock().unwrap().insert(sb.id.clone(), handle);
        Ok(())
    }

    fn assign_forwards(&self, existing: &[Forward], specs: &[String]) -> Result<Vec<Forward>> {
        crate::forward::assign(&self.cfg.forward_range()?, existing, specs)
    }

    pub async fn update_forwards(&self, selector: &str, add: &[String], remove: &[u16]) -> Result<Sandbox> {
        let mut sb = self.resolve(selector)?;
        let _busy = self.claim(&sb.id)?;
        for port in remove {
            if !sb.forwards.iter().any(|f| f.listen == *port) {
                bail!("sandbox {} has no forward on port {port}", sb.id);
            }
        }
        sb.forwards.retain(|f| !remove.contains(&f.listen));
        let added = self.assign_forwards(&sb.forwards, add)?;

        let ctx = self.proxies.lock().unwrap().get(&sb.id).map(|h| {
            for port in remove {
                h.detach_forward(*port);
            }
            h.forward_context()
        });
        if let Some((gateway, ctx)) = ctx {
            for f in &added {
                let task = crate::forward::spawn(gateway, f.clone(), ctx.clone()).await?;
                if let Some(h) = self.proxies.lock().unwrap().get(&sb.id) {
                    h.attach_forward(f.listen, task);
                }
            }
        }
        sb.forwards.extend(added);
        self.store.put(&sb)?;
        self.store.event(&sb.id, "forward", &format!("add={add:?} remove={remove:?}"));
        Ok(sb)
    }

    /// Put the proxy settings in `/etc/profile.d` so login shells and tools
    /// started outside `sandbox exec` pick them up too.
    async fn write_profile(&self, sb: &Sandbox) {
        let exports: String = sb
            .exec_env()
            .iter()
            .filter(|e| e.contains('='))
            .filter_map(|e| e.split_once('='))
            .map(|(k, v)| format!("export {k}='{}'\n", v.replace('\'', r"'\''")))
            .collect();
        let script = format!(
            "mkdir -p /etc/profile.d && cat > /etc/profile.d/sandbox.sh <<'SANDBOX_EOF'\n{exports}SANDBOX_EOF\n"
        );
        if let Err(e) = self.rt.exec(&sb.container, &[], &script).await {
            tracing::warn!(id = %sb.id, error = %format!("{e:#}"), "could not write /etc/profile.d/sandbox.sh");
        }
    }

    pub async fn destroy(&self, selector: &str, discard: bool) -> Result<Sandbox> {
        let mut sb = self.resolve(selector)?;
        let _busy = self.claim(&sb.id)?;
        let mode = if discard { Dirty::Discard } else { Dirty::Refuse };
        self.teardown(&mut sb, mode).await?;
        Ok(sb)
    }

    /// Remove the proxy, container, and network, then deal with the workspace.
    /// Leaves `sb` either `Destroyed` or `Stopped` (workspace kept).
    async fn teardown(&self, sb: &mut Sandbox, dirty: Dirty) -> Result<()> {
        let dirty_files = match &sb.workspace {
            Some(ws) => self.ws.dirty_files(ws).await.unwrap_or_default(),
            None => Vec::new(),
        };
        let keep_workspace = !dirty_files.is_empty() && dirty != Dirty::Discard;
        if keep_workspace && dirty == Dirty::Refuse {
            let ws = sb.workspace.as_ref().unwrap();
            let branch = match ws {
                Workspace::Worktree { branch, .. } => format!(" on branch {branch}"),
                Workspace::Scratch { .. } => String::new(),
            };
            bail!(
                "sandbox {} has {} uncommitted change(s) in {}{branch}.\n\
                 Commit them (from `sandbox shell {}` or on the host), or pass --discard to throw them away.",
                sb.id,
                dirty_files.len(),
                ws.path().display(),
                sb.id,
            );
        }

        self.proxies.lock().unwrap().remove(&sb.id);
        self.rt.stop_and_delete(&sb.container).await?;
        if let Err(e) = self.rt.delete_network(&sb.network).await {
            // Not fatal: reconciliation will retry the delete later.
            tracing::warn!(id = %sb.id, error = %format!("{e:#}"), "network delete failed");
        }

        if let Some(Workspace::Worktree { git_common_dir, .. }) = &sb.workspace {
            self.enforce_guard(&sb.id, git_common_dir).await;
        }

        for note in agents::sync_back(&sb.id, &sb.agent_mounts) {
            tracing::info!(id = %sb.id, "{note}");
            self.store.event(&sb.id, "sync_back", &note);
        }
        sb.agent_mounts.clear();
        let state_dir = self.agent_state_dir(&sb.id);
        if state_dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&state_dir) {
                tracing::warn!(id = %sb.id, error = %e, "could not remove agent state");
            }
        }

        if keep_workspace {
            sb.state = SandboxState::Stopped;
            sb.error = Some(format!(
                "container stopped; workspace kept with {} uncommitted change(s)",
                dirty_files.len()
            ));
            self.store.put(sb)?;
            self.store.event(&sb.id, "stopped", "workspace kept (dirty)");
            return Ok(());
        }

        if let Some(ws) = &sb.workspace {
            self.ws.release(&sb.id, ws).await?;
        }
        self.guard.forget(&sb.id);
        sb.state = SandboxState::Destroyed;
        self.store.put(sb)?;
        self.store.event(&sb.id, "destroyed", "");
        tracing::info!(id = %sb.id, "sandbox destroyed");
        Ok(())
    }

    fn agent_state_dir(&self, id: &str) -> std::path::PathBuf {
        self.paths.home.join("agent-state").join(id)
    }

    async fn enforce_guard(&self, id: &str, git_common_dir: &Path) {
        match self.guard.enforce(id, git_common_dir).await {
            Ok(keys) if !keys.is_empty() => {
                let detail = keys.join(", ");
                tracing::warn!(id, keys = %detail, "reverted execution-capable git config added from inside the sandbox");
                self.store.event(id, "git_guard", &detail);
            }
            Ok(_) => {}
            Err(e) => tracing::warn!(id, error = %format!("{e:#}"), "git guard check failed"),
        }
    }

    pub fn update_policy(&self, selector: &str, add: &[String], remove: &[String]) -> Result<Sandbox> {
        let mut sb = self.resolve(selector)?;
        let mut allow: BTreeSet<String> = sb.allow.iter().cloned().collect();
        for entry in remove {
            if !allow.remove(entry.trim()) {
                bail!("{entry:?} is not in the allowlist of sandbox {}", sb.id);
            }
        }
        allow.extend(add.iter().map(|s| s.trim().to_string()));
        let allow: Vec<String> = allow.into_iter().collect();
        self.set_proxy_policy(&sb.id, Policy::new(&allow)?);
        sb.allow = allow;
        self.store.put(&sb)?;
        self.store.event(&sb.id, "policy", &format!("add={add:?} remove={remove:?}"));
        Ok(sb)
    }

    /// Bring runtime state back in line with the database after a restart:
    /// re-attach proxies to running sandboxes, tear down the ones whose
    /// containers died, and remove orphaned containers and networks.
    pub async fn reconcile(&self) -> Result<()> {
        self.rt.check().await?;
        for mut sb in self.store.live()? {
            let _busy = self.claim(&sb.id)?;
            let status = self.rt.inspect(&sb.container).await?;
            let expired = sb.expires_at.is_some_and(|t| t <= Utc::now());
            match (sb.state, status) {
                (SandboxState::Running, Some(st)) if st.running && !expired => {
                    let (Some(ip), Some(gw)) = (st.ip, st.gateway) else { continue };
                    match self.start_proxy(&sb, gw, ip).await {
                        Ok(()) => tracing::info!(id = %sb.id, "re-attached to running sandbox"),
                        Err(e) => tracing::error!(id = %sb.id, error = %format!("{e:#}"), "re-attach failed"),
                    }
                }
                (SandboxState::Stopped, _) => {}
                _ => {
                    tracing::info!(id = %sb.id, state = %sb.state, expired, "recovering abandoned sandbox");
                    if let Err(e) = self.teardown(&mut sb, Dirty::Keep).await {
                        tracing::error!(id = %sb.id, error = %format!("{e:#}"), "recovery failed");
                    }
                }
            }
        }

        let running: HashSet<String> = self
            .store
            .live()?
            .into_iter()
            .filter(|s| s.state == SandboxState::Running)
            .map(|s| s.id)
            .collect();
        for (name, id) in self.rt.managed_containers().await? {
            if !running.contains(&id) {
                tracing::info!(container = %name, "removing orphaned container");
                if let Err(e) = self.rt.stop_and_delete(&name).await {
                    tracing::warn!(container = %name, error = %format!("{e:#}"), "orphan removal failed");
                }
            }
        }
        for (name, id) in self.rt.managed_networks().await? {
            if !running.contains(&id) {
                tracing::info!(network = %name, "removing orphaned network");
                if let Err(e) = self.rt.delete_network(&name).await {
                    tracing::warn!(network = %name, error = %format!("{e:#}"), "orphan network removal failed");
                }
            }
        }
        Ok(())
    }

    /// Periodic housekeeping: git guard every tick, expiry and liveness less often.
    pub async fn reap_loop(self: Arc<Self>) {
        let mut tick: u64 = 0;
        let mut interval = tokio::time::interval(Duration::from_secs(3));
        loop {
            interval.tick().await;
            tick += 1;
            let live = match self.store.live() {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!(error = %e, "cannot read state");
                    continue;
                }
            };
            for sb in live.iter().filter(|s| s.state == SandboxState::Running) {
                if let Some(Workspace::Worktree { git_common_dir, .. }) = &sb.workspace {
                    self.enforce_guard(&sb.id, git_common_dir).await;
                }
            }
            if !tick.is_multiple_of(10) {
                continue;
            }
            for mut sb in live.into_iter().filter(|s| s.state == SandboxState::Running) {
                let Ok(_busy) = self.claim(&sb.id) else { continue };
                let expired = sb.expires_at.is_some_and(|t| t <= Utc::now());
                // Only a definite answer counts as exited: a failed inspect
                // (runtime busy or restarting) is retried on the next pass.
                let alive = match self.rt.inspect(&sb.container).await {
                    Ok(st) => st.is_some_and(|st| st.running),
                    Err(e) => {
                        tracing::warn!(id = %sb.id, error = %format!("{e:#}"), "cannot inspect container, will retry");
                        true
                    }
                };
                if expired || !alive {
                    let why = if expired { "expired" } else { "container exited" };
                    tracing::info!(id = %sb.id, why, "tearing down sandbox");
                    self.store.event(&sb.id, "reap", why);
                    if let Err(e) = self.teardown(&mut sb, Dirty::Keep).await {
                        tracing::error!(id = %sb.id, error = %format!("{e:#}"), "teardown failed");
                    }
                }
            }
        }
    }
}

pub fn ensure_dirs(paths: &Paths) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    for dir in [&paths.home, &paths.logs, &paths.workspaces(), &paths.policies()] {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    // The socket lives here; nobody else gets to traverse to it.
    std::fs::set_permissions(&paths.home, std::fs::Permissions::from_mode(0o700))?;
    Ok(())
}
