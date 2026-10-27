//! `sandbox`: host-side CLI for sandboxd.
//!
//! Lifecycle and policy go through the daemon. Interactive sessions (`shell`,
//! `exec`) are attached directly with `container exec`, using the container
//! and environment the daemon reports, so TTYs behave natively.

use std::collections::BTreeMap;
use std::io::{BufRead, IsTerminal, Seek};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use sandbox_core::api::{CreateRequest, Request, Sandbox, SandboxState, Workspace, WORKSPACE_MOUNT};
use sandbox_core::client;
use sandbox_core::paths::Paths;
use serde::Deserialize;

#[derive(Parser)]
#[command(name = "sandbox", version, about = "Run agents in Apple Container sandboxes with allowlisted egress")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a sandbox. Inside a git repo this leases a treehouse worktree on
    /// a new `sandbox/<id>` branch and mounts it at /workspace.
    Create(Box<CreateArgs>),
    /// List sandboxes.
    #[command(alias = "ls")]
    List,
    /// Show a sandbox's full record as JSON.
    Info { sandbox: Option<String> },
    /// Open an interactive shell inside a sandbox.
    Shell { sandbox: Option<String> },
    /// Run the sandbox's agent: `sandbox agent [ID] [-- extra args]`.
    Agent {
        sandbox: Option<String>,
        /// Which attached agent, if there are several.
        #[arg(short = 'A', long = "agent")]
        agent: Option<String>,
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// Run a command inside a sandbox: `sandbox exec <id> -- pi "fix the tests"`.
    Exec {
        sandbox: Option<String>,
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Allow a sandbox to reach more hosts (takes effect immediately).
    Allow {
        sandbox: String,
        #[arg(required = true)]
        hosts: Vec<String>,
    },
    /// Remove hosts from a sandbox's allowlist (new connections only).
    Deny {
        sandbox: String,
        #[arg(required = true)]
        hosts: Vec<String>,
    },
    /// Show a sandbox's egress log (every proxied connection, allowed or denied).
    Logs {
        sandbox: Option<String>,
        /// Keep printing new entries.
        #[arg(short, long)]
        follow: bool,
        /// Only show blocked or failed requests.
        #[arg(long)]
        denied: bool,
        /// Print raw JSON lines.
        #[arg(long)]
        json: bool,
    },
    /// Destroy a sandbox and return its worktree to the treehouse pool.
    /// Refuses if the worktree has uncommitted changes, unless --discard.
    #[command(alias = "rm")]
    Destroy {
        sandbox: Option<String>,
        /// Throw away uncommitted changes in the workspace.
        #[arg(long)]
        discard: bool,
    },
    /// List the allowlist profiles available to `create --profile`.
    Profiles,
    /// List the agent presets available to `create --agent`.
    Agents,
    /// Check that sandboxd is reachable.
    Status,
}

#[derive(Args)]
struct CreateArgs {
    /// Hosts the sandbox may reach (repeatable). See `sandbox profiles` for bundles.
    #[arg(short, long = "allow", value_name = "HOST")]
    allow: Vec<String>,
    /// Allowlist profile from policies/*.toml (repeatable).
    #[arg(short, long = "profile", value_name = "NAME")]
    profile: Vec<String>,
    /// Agent preset from agents/*.toml (repeatable): adds its allowlist,
    /// forwarded env vars, and a copy of its host config.
    #[arg(short = 'A', long = "agent", value_name = "NAME")]
    agent: Vec<String>,
    /// Don't apply `default_agents` from config.toml.
    #[arg(long, conflicts_with = "agent")]
    no_agent: bool,
    /// Repository to cut the worktree from (default: the repo containing the cwd).
    #[arg(long, conflicts_with = "scratch")]
    repo: Option<PathBuf>,
    /// Use an empty scratch workspace instead of a worktree.
    #[arg(long)]
    scratch: bool,
    /// Branch to cut the worktree from (default: treehouse's inferred base).
    #[arg(long)]
    base: Option<String>,
    /// Name of the new branch (default: sandbox/<id>).
    #[arg(short, long)]
    branch: Option<String>,
    /// A friendly name usable instead of the id.
    #[arg(short, long)]
    name: Option<String>,
    #[arg(long)]
    image: Option<String>,
    #[arg(long)]
    cpus: Option<u32>,
    /// Memory limit, e.g. 4G.
    #[arg(long)]
    memory: Option<String>,
    /// Destroy after this long, e.g. 30m or 2h ("0" = never).
    #[arg(long)]
    ttl: Option<String>,
    /// KEY=VALUE to set, or KEY to forward from your environment on every
    /// exec/shell without storing it (use this for API keys). Repeatable.
    #[arg(short, long = "env", value_name = "KEY[=VALUE]")]
    env: Vec<String>,
    /// Open a shell in the sandbox once it is up.
    #[arg(long, conflicts_with = "run")]
    shell: bool,
    /// Start the agent once the sandbox is up.
    #[arg(long)]
    run: bool,
    /// With --shell or --run, destroy the sandbox when it exits.
    #[arg(long)]
    rm: bool,
    /// Print only the sandbox id.
    #[arg(short, long)]
    quiet: bool,
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("sandbox: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let socket = Paths::resolve().socket();
    let call = |req: Request| -> Result<serde_json::Value> { rt.block_on(client::call(&socket, &req)) };
    let get = |sel: Option<String>| -> Result<Sandbox> { resolve(&call, sel) };

    match cli.cmd {
        Cmd::Create(args) => return create(&call, *args),
        Cmd::List => {
            let list: Vec<Sandbox> = serde_json::from_value(call(Request::List)?)?;
            print_table(&list);
        }
        Cmd::Info { sandbox } => println!("{}", serde_json::to_string_pretty(&get(sandbox)?)?),
        Cmd::Shell { sandbox } => return shell(&get(sandbox)?),
        Cmd::Agent { sandbox, agent, args } => return run_agent(&get(sandbox)?, agent.as_deref(), &args),
        Cmd::Exec { sandbox, command } => return exec(&get(sandbox)?, &command),
        Cmd::Allow { sandbox, hosts } => {
            let sb: Sandbox =
                serde_json::from_value(call(Request::Policy { selector: sandbox, add: hosts, remove: vec![] })?)?;
            println!("{} may now reach: {}", sb.id, sb.allow.join(", "));
        }
        Cmd::Deny { sandbox, hosts } => {
            let sb: Sandbox =
                serde_json::from_value(call(Request::Policy { selector: sandbox, add: vec![], remove: hosts })?)?;
            println!("{} may now reach: {}", sb.id, display_allow(&sb.allow));
        }
        Cmd::Logs { sandbox, follow, denied, json } => logs(&get(sandbox)?, follow, denied, json)?,
        Cmd::Destroy { sandbox, discard } => {
            let sb = get(sandbox)?;
            destroy(&call, &sb.id, discard)?;
        }
        Cmd::Profiles => {
            #[derive(Deserialize)]
            struct Profile {
                description: String,
                allow: Vec<String>,
            }
            let profiles: BTreeMap<String, Profile> = serde_json::from_value(call(Request::Profiles)?)?;
            for (name, p) in profiles {
                println!("{name:<10} {}\n           {}", p.description, p.allow.join(", "));
            }
        }
        Cmd::Agents => {
            #[derive(Deserialize)]
            struct Preset {
                description: String,
                command: Vec<String>,
                #[serde(default)]
                allow: Vec<String>,
                #[serde(default)]
                profiles: Vec<String>,
            }
            let presets: BTreeMap<String, Preset> = serde_json::from_value(call(Request::Agents)?)?;
            for (name, p) in presets {
                let mut egress = p.profiles.iter().map(|p| format!("@{p}")).collect::<Vec<_>>();
                egress.extend(p.allow);
                println!("{name:<10} {} (runs `{}`)\n           {}", p.description, p.command.join(" "), egress.join(", "));
            }
        }
        Cmd::Status => {
            let version: String = serde_json::from_value(call(Request::Ping)?)?;
            println!("sandboxd {version} is running ({})", socket.display());
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Pick a sandbox: an explicit selector, else the one whose workspace holds
/// the current directory, else the only one that exists.
fn resolve(call: &dyn Fn(Request) -> Result<serde_json::Value>, sel: Option<String>) -> Result<Sandbox> {
    if let Some(selector) = sel {
        return Ok(serde_json::from_value(call(Request::Get { selector })?)?);
    }
    if let Ok(cwd) = std::env::current_dir() {
        if let Ok(v) = call(Request::Get { selector: cwd.display().to_string() }) {
            return Ok(serde_json::from_value(v)?);
        }
    }
    Ok(serde_json::from_value(call(Request::Get { selector: String::new() })?)?)
}

fn create(call: &dyn Fn(Request) -> Result<serde_json::Value>, args: CreateArgs) -> Result<ExitCode> {
    if args.rm && !args.shell && !args.run {
        bail!("--rm needs --shell or --run");
    }
    let repo = if args.scratch {
        None
    } else if let Some(repo) = args.repo {
        Some(std::fs::canonicalize(&repo).with_context(|| format!("{} does not exist", repo.display()))?)
    } else {
        git_toplevel(&std::env::current_dir()?)
    };
    let ttl_secs = args
        .ttl
        .as_deref()
        .map(|t| if t.trim() == "0" { Ok(0) } else { humantime::parse_duration(t).map(|d| d.as_secs()) })
        .transpose()
        .context("invalid --ttl")?;

    let mut env = git_identity();
    for e in &args.env {
        if !e.contains('=') && std::env::var_os(e).is_none() {
            eprintln!("sandbox: warning: {e} is not set in your environment");
        }
    }
    env.extend(args.env);

    if !args.quiet {
        match &repo {
            Some(r) => eprintln!("Creating sandbox with a worktree of {}...", r.display()),
            None => eprintln!("Creating sandbox with a scratch workspace..."),
        }
    }
    let req = CreateRequest {
        name: args.name,
        repo,
        base: args.base,
        branch: args.branch,
        image: args.image,
        profiles: args.profile,
        agents: args.agent,
        no_agents: args.no_agent,
        allow: args.allow,
        cpus: args.cpus,
        memory: args.memory,
        ttl_secs,
        env,
    };
    let sb: Sandbox = serde_json::from_value(call(Request::Create(req))?)?;

    if args.quiet {
        println!("{}", sb.id);
    } else {
        println!("{}", sb.id);
        if let Some(ws) = &sb.workspace {
            eprintln!("  workspace  {} -> {WORKSPACE_MOUNT}", ws.path().display());
            if let Workspace::Worktree { branch, .. } = ws {
                eprintln!("  branch     {branch}");
            }
        }
        if !sb.agents.is_empty() {
            let names: Vec<&str> = sb.agents.iter().map(|a| a.name.as_str()).collect();
            eprintln!("  agents     {}", names.join(", "));
        }
        eprintln!("  egress     {}", display_allow(&sb.allow));
        if let Some(exp) = sb.expires_at {
            eprintln!("  expires    {}", exp.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M"));
        }
        if !args.shell && !args.run {
            let first = if sb.agents.is_empty() { "shell" } else { "agent" };
            eprintln!("\n  sandbox {first} {0}    sandbox exec {0} -- <cmd>    sandbox destroy {0}", sb.id);
        }
    }

    if args.shell || args.run {
        let code = if args.run { run_agent(&sb, None, &[])? } else { shell(&sb)? };
        if args.rm {
            if let Err(e) = destroy(call, &sb.id, false) {
                eprintln!("sandbox: {e:#}");
                return Ok(ExitCode::FAILURE);
            }
        }
        return Ok(code);
    }
    Ok(ExitCode::SUCCESS)
}

fn destroy(call: &dyn Fn(Request) -> Result<serde_json::Value>, id: &str, discard: bool) -> Result<()> {
    let sb: Sandbox =
        serde_json::from_value(call(Request::Destroy { selector: id.to_string(), discard })?)?;
    match sb.state {
        SandboxState::Destroyed => {
            eprint!("Destroyed {}", sb.id);
            if let Some(Workspace::Worktree { branch, .. }) = &sb.workspace {
                eprint!("; worktree returned to the pool, commits remain on branch {branch}");
            }
            eprintln!();
        }
        other => eprintln!("{}: {other} ({})", sb.id, sb.error.unwrap_or_default()),
    }
    Ok(())
}

fn git_toplevel(dir: &Path) -> Option<PathBuf> {
    let out = Command::new("git").arg("-C").arg(dir).args(["rev-parse", "--show-toplevel"]).output().ok()?;
    out.status.success().then(|| PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}

/// Commit as the host user from inside the sandbox.
fn git_identity() -> Vec<String> {
    let get = |key: &str| -> Option<String> {
        let out = Command::new("git").args(["config", "--get", key]).output().ok()?;
        let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (out.status.success() && !v.is_empty()).then_some(v)
    };
    let mut env = Vec::new();
    if let Some(name) = get("user.name") {
        env.push(format!("GIT_AUTHOR_NAME={name}"));
        env.push(format!("GIT_COMMITTER_NAME={name}"));
    }
    if let Some(email) = get("user.email") {
        env.push(format!("GIT_AUTHOR_EMAIL={email}"));
        env.push(format!("GIT_COMMITTER_EMAIL={email}"));
    }
    env
}

fn require_running(sb: &Sandbox) -> Result<()> {
    if sb.state != SandboxState::Running {
        bail!("sandbox {} is {} ({})", sb.id, sb.state, sb.error.clone().unwrap_or_default());
    }
    Ok(())
}

fn container_exec(sb: &Sandbox, tty: bool) -> Command {
    let mut cmd = Command::new(std::env::var("SANDBOX_CONTAINER_BIN").unwrap_or_else(|_| "container".into()));
    cmd.args(["exec", "--interactive", "--workdir", WORKSPACE_MOUNT]);
    if tty {
        cmd.arg("--tty");
        if let Ok(term) = std::env::var("TERM") {
            cmd.args(["--env", &format!("TERM={term}")]);
        }
    }
    // Bare `KEY` entries make `container` copy the value from our
    // environment; skip the ones that aren't set here.
    for e in sb.exec_env() {
        if e.contains('=') || std::env::var_os(&e).is_some() {
            cmd.args(["--env", &e]);
        }
    }
    cmd.arg(&sb.container);
    cmd
}

fn exit_code(status: std::process::ExitStatus) -> ExitCode {
    ExitCode::from(status.code().unwrap_or(1).clamp(0, 255) as u8)
}

fn shell(sb: &Sandbox) -> Result<ExitCode> {
    require_running(sb)?;
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let status = container_exec(sb, tty)
        .args(["/bin/sh", "-c", "if command -v bash >/dev/null 2>&1; then exec bash -l; else exec sh -l; fi"])
        .status()
        .context("failed to run `container exec`")?;
    Ok(exit_code(status))
}

fn run_agent(sb: &Sandbox, name: Option<&str>, extra: &[String]) -> Result<ExitCode> {
    let agent = match name {
        Some(n) => sb.agents.iter().find(|a| a.name == n),
        None => sb.agents.first(),
    };
    let Some(agent) = agent else {
        match name {
            Some(n) => bail!("agent {n:?} is not attached to sandbox {}", sb.id),
            None => bail!("sandbox {} has no agent; create it with --agent <name> (see `sandbox agents`)", sb.id),
        }
    };
    let mut command = agent.command.clone();
    command.extend(extra.iter().cloned());
    exec(sb, &command)
}

fn exec(sb: &Sandbox, command: &[String]) -> Result<ExitCode> {
    require_running(sb)?;
    let tty = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    let status = container_exec(sb, tty).args(command).status().context("failed to run `container exec`")?;
    Ok(exit_code(status))
}

fn display_allow(allow: &[String]) -> String {
    if allow.is_empty() { "(nothing; all egress blocked)".into() } else { allow.join(", ") }
}

fn print_table(list: &[Sandbox]) {
    if list.is_empty() {
        println!("No sandboxes.");
        return;
    }
    println!("{:<8} {:<14} {:<9} {:<28} {:<17} EGRESS", "ID", "NAME", "STATE", "BRANCH/WORKSPACE", "EXPIRES");
    for sb in list {
        let ws = match &sb.workspace {
            Some(Workspace::Worktree { branch, .. }) => branch.clone(),
            Some(Workspace::Scratch { .. }) => "(scratch)".into(),
            None => "-".into(),
        };
        let expires = sb
            .expires_at
            .map(|t| t.with_timezone(&chrono::Local).format("%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "never".into());
        let egress = match sb.allow.len() {
            0 => "blocked".into(),
            n if n <= 2 => sb.allow.join(","),
            n => format!("{},+{}", sb.allow[0], n - 1),
        };
        println!(
            "{:<8} {:<14} {:<9} {:<28} {:<17} {egress}",
            sb.id,
            sb.name.as_deref().unwrap_or("-"),
            sb.state.to_string(),
            ws,
            expires
        );
    }
}

#[derive(Deserialize)]
struct EgressEvent {
    ts: String,
    #[serde(default)]
    event: String,
    method: String,
    host: String,
    port: u16,
    allowed: bool,
    reason: Option<String>,
    bytes_up: Option<u64>,
    bytes_down: Option<u64>,
}

fn logs(sb: &Sandbox, follow: bool, denied: bool, json: bool) -> Result<()> {
    let path = &sb.egress_log;
    let mut file = loop {
        match std::fs::File::open(path) {
            Ok(f) => break f,
            Err(_) if follow => std::thread::sleep(Duration::from_millis(500)),
            Err(_) => {
                eprintln!("No egress recorded for {} yet.", sb.id);
                return Ok(());
            }
        }
    };
    let mut pos = 0u64;
    loop {
        file.seek(std::io::SeekFrom::Start(pos))?;
        let mut reader = std::io::BufReader::new(&file);
        let mut line = String::new();
        while reader.read_line(&mut line)? > 0 {
            if !line.ends_with('\n') {
                break; // partial write; pick it up next round
            }
            pos += line.len() as u64;
            print_event(line.trim_end(), denied, json);
            line.clear();
        }
        if !follow {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn print_event(line: &str, denied_only: bool, json: bool) {
    let Ok(ev) = serde_json::from_str::<EgressEvent>(line) else { return };
    if denied_only && ev.allowed {
        return;
    }
    if json {
        println!("{line}");
        return;
    }
    let ts = chrono::DateTime::parse_from_rfc3339(&ev.ts)
        .map(|t| t.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
        .unwrap_or(ev.ts);
    let verdict = match ev.event.as_str() {
        "open" => "ALLOW",
        "close" => "CLOSE",
        "error" => "ERROR",
        _ => "DENY ",
    };
    let detail = match (ev.allowed, ev.bytes_up, ev.bytes_down) {
        (true, Some(up), Some(down)) => format!("↑{} ↓{}", human_bytes(up), human_bytes(down)),
        _ => ev.reason.unwrap_or_default(),
    };
    println!("{ts}  {verdict}  {:<7} {}:{}  {detail}", ev.method, ev.host, ev.port);
}

fn human_bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1}M", n as f64 / (1 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1}K", n as f64 / (1 << 10) as f64),
        n => format!("{n}B"),
    }
}
