//! Agent presets (`agents/<name>.toml`): what to run, what it may reach, which
//! credentials to forward, and which host config to bring along.
//!
//! Host config is copied, never mounted, so an agent can't plant extensions,
//! hooks, or skills that would later run on the host. Files listed in
//! `sync_back` (refreshed OAuth tokens, typically) are copied home on teardown
//! if only the sandbox changed them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sandbox_core::api::AgentMount;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Preset {
    #[serde(default)]
    pub description: String,
    /// What `sandbox agent` runs inside `/workspace`.
    pub command: Vec<String>,
    /// Allowlist profiles from policies/*.toml.
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub allow: Vec<String>,
    /// Bare `KEY`s are forwarded from your shell on each exec when set;
    /// `KEY=VALUE` is set on the container.
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub config: Vec<ConfigDir>,
    /// Shell commands run once in `/workspace` after the sandbox starts, e.g.
    /// to install plugins. Creation fails if one fails.
    #[serde(default)]
    pub setup: Vec<String>,
    /// Hosts reachable only while `setup` runs.
    #[serde(default)]
    pub setup_allow: Vec<String>,
    /// `[PORT=]HOST:PORT` services relayed into the sandbox (see `--forward`).
    #[serde(default)]
    pub forward: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigDir {
    /// Directory on the host; `~` expands to your home. Skipped if missing.
    pub host: String,
    /// Where the copy is mounted in the container.
    pub container: PathBuf,
    /// Top-level entries to copy. Empty means everything not excluded.
    #[serde(default)]
    pub include: Vec<String>,
    /// Top-level entries to skip.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Top-level files copied back to the host on teardown when changed in the
    /// sandbox and unchanged on the host.
    #[serde(default)]
    pub sync_back: Vec<String>,
}

pub fn load(dir: &Path) -> Result<BTreeMap<String, Preset>> {
    std::fs::create_dir_all(dir)?;
    if std::fs::read_dir(dir)?.next().is_none() {
        for (name, body) in DEFAULT_PRESETS {
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
        let preset: Preset = toml::from_str(&std::fs::read_to_string(&path)?)
            .with_context(|| format!("invalid {}", path.display()))?;
        if preset.command.is_empty() {
            bail!("{}: command must not be empty", path.display());
        }
        out.insert(name, preset);
    }
    Ok(out)
}

fn expand_home(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default().join(rest),
        None => PathBuf::from(p),
    }
}

/// Copy each config dir of `preset` under `state_dir` and describe the mounts.
pub fn seed(agent: &str, preset: &Preset, state_dir: &Path) -> Result<Vec<AgentMount>> {
    let mut mounts = Vec::new();
    for (i, cfg) in preset.config.iter().enumerate() {
        let host = expand_home(&cfg.host);
        if !host.is_dir() {
            tracing::info!(agent, host = %host.display(), "agent config dir missing, skipping");
            continue;
        }
        let base = state_dir.join(format!("{agent}-{i}"));
        let data = base.join("data");
        let pristine = base.join("seed");
        std::fs::create_dir_all(&data)?;
        std::fs::create_dir_all(&pristine)?;
        for entry in std::fs::read_dir(&host)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let wanted = (cfg.include.is_empty() || cfg.include.contains(&name)) && !cfg.exclude.contains(&name);
            if wanted {
                copy_tree(&entry.path(), &data.join(&name), 0)
                    .with_context(|| format!("copy {}", entry.path().display()))?;
            }
        }
        for name in &cfg.sync_back {
            let src = data.join(name);
            if src.is_file() {
                std::fs::copy(&src, pristine.join(name))?;
            }
        }
        mounts.push(AgentMount {
            agent: agent.to_string(),
            host,
            data,
            seed: pristine,
            container: cfg.container.clone(),
            sync_back: cfg.sync_back.clone(),
        });
    }
    Ok(mounts)
}

/// Recursive copy that follows symlinks (skills are often linked in from
/// elsewhere) and skips sockets, FIFOs, broken links, and Finder litter.
fn copy_tree(src: &Path, dst: &Path, depth: usize) -> Result<()> {
    if depth > 32 {
        bail!("{} is nested too deeply (symlink loop?)", src.display());
    }
    if src.file_name().is_some_and(|n| n == ".DS_Store") {
        return Ok(());
    }
    let Ok(meta) = std::fs::metadata(src) else { return Ok(()) };
    if meta.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()), depth + 1)?;
        }
    } else if meta.is_file() {
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

/// Copy changed `sync_back` files home. Returns human-readable notes.
pub fn sync_back(id: &str, mounts: &[AgentMount]) -> Vec<String> {
    let mut notes = Vec::new();
    for m in mounts {
        for name in &m.sync_back {
            let (inside, pristine, home) = (m.data.join(name), m.seed.join(name), m.host.join(name));
            let read = |p: &Path| std::fs::read(p).ok();
            let Some(new) = read(&inside) else { continue };
            let seeded = read(&pristine);
            if seeded.as_deref() == Some(new.as_slice()) {
                continue;
            }
            if read(&home) != seeded {
                notes.push(format!(
                    "{}: {} changed both on the host and in sandbox {id}; kept the host copy",
                    m.agent,
                    home.display()
                ));
                continue;
            }
            let tmp = home.with_extension(format!("sandbox-{id}.tmp"));
            let res = std::fs::write(&tmp, &new).and_then(|_| {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
                std::fs::rename(&tmp, &home)
            });
            match res {
                Ok(()) => notes.push(format!("{}: updated {}", m.agent, home.display())),
                Err(e) => notes.push(format!("{}: could not update {}: {e}", m.agent, home.display())),
            }
        }
    }
    notes
}

const DEFAULT_PRESETS: &[(&str, &str)] = &[
    (
        "pi",
        r#"description = "pi coding agent"
command = ["pi"]

# Hosts pi talks to. Add your own providers (e.g. a LiteLLM gateway) here.
allow = [
  "api.anthropic.com",
  "api.openai.com",
  "chatgpt.com",                        # openai-codex (ChatGPT login)
  "auth.openai.com",
  "generativelanguage.googleapis.com",
  "openrouter.ai",
]

# Forwarded from your shell when set; never stored.
env = [
  "ANTHROPIC_API_KEY", "ANTHROPIC_OAUTH_TOKEN", "OPENAI_API_KEY", "GEMINI_API_KEY",
  "OPENROUTER_API_KEY", "GROQ_API_KEY", "XAI_API_KEY", "DEEPSEEK_API_KEY", "MISTRAL_API_KEY",
]

# pi installs the npm: packages listed in settings.json at startup. Do it once
# here, with the registry reachable only during setup.
setup = [
  """node -e '
    const s = JSON.parse(require("fs").readFileSync("/root/.pi/agent/settings.json", "utf8"));
    for (const p of s.packages || []) {
      const src = typeof p === "string" ? p : p.source;
      if (src && src.startsWith("npm:")) console.log(src.slice(4));
    }' 2>/dev/null | xargs -r npm install -g --no-fund --no-audit --loglevel=error""",
]
setup_allow = ["registry.npmjs.org"]

# Your ~/.pi/agent (settings, models, auth, extensions, skills, prompts) is
# copied in. Sessions stay on the host; bin/ holds macOS binaries.
[[config]]
host = "~/.pi/agent"
container = "/root/.pi/agent"
exclude = ["sessions", "bin"]
sync_back = ["auth.json"]
"#,
    ),
    (
        "claude",
        r#"description = "Claude Code"
command = ["claude"]
profiles = ["claude"]

# On macOS Claude Code keeps its login in the Keychain, which can't be copied.
# Use an API key, or a long-lived token from `claude setup-token`.
env = ["ANTHROPIC_API_KEY", "CLAUDE_CODE_OAUTH_TOKEN"]

# Skip the first-run prompts (theme, login method, folder trust). Nothing from
# your ~/.claude.json is copied: it holds no credentials, just caches and
# history for every project on your Mac.
setup = [
  """[ -e /root/.claude.json ] || echo '{"hasCompletedOnboarding":true,"theme":"dark","projects":{"/workspace":{"hasTrustDialogAccepted":true}}}' > /root/.claude.json""",
]

[[config]]
host = "~/.claude"
container = "/root/.claude"
include = ["settings.json", "CLAUDE.md", "agents", "commands", "skills", "output-styles"]
"#,
    ),
    (
        "codex",
        r#"description = "OpenAI Codex CLI"
command = ["codex"]
profiles = ["codex"]
env = ["OPENAI_API_KEY"]

[[config]]
host = "~/.codex"
container = "/root/.codex"
include = ["auth.json", "config.toml", "AGENTS.md", "prompts", "skills", "rules"]
sync_back = ["auth.json"]
"#,
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_presets_parse() {
        for (name, body) in DEFAULT_PRESETS {
            let p: Preset = toml::from_str(body).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(!p.command.is_empty(), "{name}");
        }
        let pi: Preset = toml::from_str(DEFAULT_PRESETS[0].1).unwrap();
        assert_eq!(pi.setup.len(), 1);
        assert_eq!(pi.config[0].sync_back, ["auth.json"]);
    }

    #[test]
    fn sync_back_only_when_host_unchanged() {
        let root = std::env::temp_dir().join(format!("sandboxd-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (host, data, seed) = (root.join("host"), root.join("data"), root.join("seed"));
        for d in [&host, &data, &seed] {
            std::fs::create_dir_all(d).unwrap();
        }
        let m = AgentMount {
            agent: "pi".into(),
            host: host.clone(),
            data: data.clone(),
            seed: seed.clone(),
            container: "/x".into(),
            sync_back: vec!["auth.json".into()],
        };
        let put = |d: &Path, v: &str| std::fs::write(d.join("auth.json"), v).unwrap();
        let get = |d: &Path| std::fs::read_to_string(d.join("auth.json")).unwrap();

        // Untouched in the sandbox: nothing happens.
        put(&host, "v1");
        put(&seed, "v1");
        put(&data, "v1");
        assert!(sync_back("t", std::slice::from_ref(&m)).is_empty());

        // Refreshed in the sandbox only: copied home.
        put(&data, "v2");
        assert_eq!(sync_back("t", std::slice::from_ref(&m)).len(), 1);
        assert_eq!(get(&host), "v2");

        // Changed on both sides: the host wins.
        put(&host, "host");
        put(&seed, "v1");
        put(&data, "sandbox");
        let notes = sync_back("t", std::slice::from_ref(&m));
        assert!(notes[0].contains("kept the host copy"), "{notes:?}");
        assert_eq!(get(&host), "host");
        let _ = std::fs::remove_dir_all(&root);
    }
}
