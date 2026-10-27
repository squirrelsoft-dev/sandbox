# sandbox

Run coding agents (Claude Code, Codex, pi, your own) in Apple Container VMs
whose only way out is an allowlisting egress proxy, each working in its own
treehouse worktree.

```
sandbox create -A pi --run              # worktree of the current repo, start pi in it
sandbox agent 3fa2c1 -- -p "fix the failing tests"
sandbox shell 3fa2c1                    # poke around next to the agent
sandbox logs 3fa2c1 --denied
sandbox allow 3fa2c1 registry.npmjs.org
sandbox destroy 3fa2c1
```

## Install and usage

### Requirements

- **A Mac with Apple silicon** (M1 or newer). Apple's container runtime
  doesn't run on Intel Macs.
- **macOS 26 (Tahoe) or newer.** Tested on macOS 27.
- **An administrator account.** Installing the firewall asks for your
  password (`sudo`).
- **Room to run VMs.** Each sandbox gets 4 CPUs and 4 GB of memory by default
  (change with `--cpus`/`--memory` or in `config.toml`). The agent image
  needs a few GB of disk.
- **Software**, installed in the steps below: Xcode Command Line Tools (for
  git), Homebrew, Rust, Apple's `container` (1.6.1 or 0.10.0), and
  [treehouse](https://github.com/kunchenguid/treehouse).

### Install

Run these in Terminal, one step at a time.

**1. Install the Xcode Command Line Tools** (provides `git`). Skip this if
`git --version` already prints a version.

```sh
xcode-select --install
```

**2. Install Homebrew** if `brew --version` doesn't work. Follow the
instructions at [brew.sh](https://brew.sh), including the lines it prints at
the end to add `brew` to your PATH.

**3. Install Rust.** Accept the defaults, then close and reopen Terminal.

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

**4. Install Apple's container runtime.** Download the signed installer
(`container-<version>-installer-signed.pkg`) from
[github.com/apple/container/releases](https://github.com/apple/container/releases)
and open it. This project is tested with 1.6.1 and also supports 0.10.0.
(`brew install container` installs the same tool.) To upgrade an existing
install, run `container system stop` first, then the new installer.
Then start the runtime. The first time, it offers to download a
Linux kernel: answer `Y`.

```sh
container system start
container --version          # should print a version
```

**5. Install treehouse**, which manages the git worktrees agents work in.

```sh
brew install treehouse
```

**6. Download this project.**

```sh
git clone https://github.com/squirrelsoft-dev/sandbox.git
cd sandbox
```

**7. Build the agent image.** This is the Linux system each sandbox runs, with
Claude Code, Codex, and pi preinstalled. It takes a few minutes.

```sh
container build -t sandbox-agent:latest images/agent
```

If it fails with errors like `Temporary failure resolving 'deb.debian.org'`
(common with Tailscale or another VPN), give the builder a public DNS server
and run the build again:

```sh
container builder delete
container builder start --dns 1.1.1.1
```

Updating `container` recreates the builder without this setting, so run these
two commands again after an update if builds start failing the same way.

**8. Install sandboxd and the `sandbox` command.** This builds them, copies
them to `~/.local/bin`, and starts sandboxd in the background (it also starts
at every login).

```sh
scripts/install.sh
```

If the last line says `sandboxd 0.1.0 is running`, it worked. If your shell
can't find `sandbox` afterwards, add `~/.local/bin` to your PATH:

```sh
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc
source ~/.zshrc
```

**9. Install the firewall.** It stops sandboxes from reaching services on your
Mac and gives them DNS. It asks for your password.

```sh
sandbox firewall --install
```

**10. Check everything.**

```sh
sandbox status               # sandboxd is running
sandbox firewall             # Installed.
sandbox agents               # pi, claude, codex
```

### Give your agent credentials

Agents run inside a VM, so they can't use logins stored in your Mac's
Keychain. Set credentials in the terminal you run `sandbox` from. The CLI
passes them in each time and never saves them.

- **Claude Code** with a Claude subscription:
  ```sh
  claude setup-token                          # on your Mac, once
  export CLAUDE_CODE_OAUTH_TOKEN=<the token>  # add to ~/.zshrc to keep it
  ```
  With API billing, use `export ANTHROPIC_API_KEY=<key>` instead.
- **Codex**: `export OPENAI_API_KEY=<key>`, or log in with `codex` on your Mac
  first (its `~/.codex/auth.json` is copied in).
- **pi**: log in with `pi` on your Mac first. Its `~/.pi/agent` config is
  copied in. Provider API keys such as `ANTHROPIC_API_KEY` are passed through
  when set.

### Your first sandbox

**1. Go to a git repository** you want the agent to work on, and commit or
stash anything you care about. The agent works on a separate copy (a
worktree) on a new branch, but it's good practice.

```sh
cd ~/code/my-project
```

**2. Create a sandbox and start the agent.**

```sh
sandbox create -A claude --run
```

This:

- creates the branch `sandbox/<id>` and a worktree of your repo, mounted at
  `/workspace` in the VM
- starts a VM that can only reach the hosts Claude Code needs
- starts Claude Code inside it, in `/workspace`

Use `-A pi` or `-A codex` for those agents. Leave out `--run` to create the
sandbox without starting the agent. Start it later with `sandbox agent`.

**3. Watch or help from a second terminal:**

```sh
sandbox shell                # a shell inside the sandbox, next to the agent
sandbox logs --denied        # sites the agent tried to reach and was refused
```

**4. Let it reach more of the internet when it needs to.** Blocked requests
fail with a message naming the host. Allow one and it works right away:

```sh
sandbox allow <id> docs.rs
```

**5. Review the work.** The agent's commits land on `sandbox/<id>` in your
repository:

```sh
git log sandbox/<id>
git diff main...sandbox/<id>      # use your default branch if it isn't main
```

**6. Clean up.** This stops the VM and removes the worktree. The branch and
its commits stay in your repo.

```sh
sandbox destroy <id>
```

If the agent left uncommitted changes, `destroy` refuses. Commit them from
`sandbox shell`, or pass `--discard` to throw them away. Sandboxes also expire
on their own after 8 hours (`--ttl` changes that).

`<id>` is the short id `create` prints, such as `3fa2c1`. You can also name a
sandbox (`-n fix-tests`) and use the name. You can leave the id out when only
one sandbox exists, or when your current directory is inside its worktree.

### Everyday commands

| Task | Command |
| --- | --- |
| Start a sandbox with an agent | `sandbox create -A claude --run` |
| Run a one-off task | `sandbox agent <id> -- -p "fix the failing tests"` |
| See your sandboxes | `sandbox ls` |
| Get a shell inside | `sandbox shell <id>` |
| Run a command inside | `sandbox exec <id> -- npm test` |
| See network activity | `sandbox logs <id>` (`-f` to follow) |
| Allow / remove a host | `sandbox allow <id> HOST` / `sandbox deny <id> HOST` |
| Reach a database on your Mac | `sandbox forward <id> localhost:5432`, then `sandbox firewall --open 15432` |
| Remove a sandbox | `sandbox destroy <id>` |

### Updating and uninstalling

To update, pull the latest version, reinstall, and rebuild the image:

```sh
cd sandbox
git pull
scripts/install.sh
sandbox firewall --install
container build -t sandbox-agent:latest images/agent
```

To uninstall, destroy your sandboxes first (`sandbox ls`, then
`sandbox destroy <id>` for each), then:

```sh
sandbox firewall --uninstall
scripts/install.sh uninstall
rm ~/.local/bin/sandbox ~/.local/bin/sandboxd
rm -rf ~/Library/Application\ Support/Sandboxd ~/Library/Logs/Sandboxd   # settings and logs
```

## Architecture

```
macOS host
├── sandbox (CLI) ──unix socket──▶ sandboxd (launchd agent)
│                                   ├── SQLite state, lifecycle events
│                                   ├── treehouse: lease / return worktrees
│                                   ├── git guard: watches the repo's .git/config
│                                   ├── reaper: TTLs, dead containers
│                                   └── egress proxy per sandbox ◀─┐
│                                                                   │ only route out
└── Apple Container runtime                                         │
    └── sbx-<id> VM on host-only network sbx-<id>-net (10.213.N.0/24)
          /workspace  = treehouse worktree (rw)
          <repo>/.git = mounted at its host path (rw), hooks/ read-only
```

The **manager decides what an agent may do and guarantees cleanup**. The
orchestrator (you, a script, another agent on the host) decides what the agent
should do. Sandboxes never see the socket, the `container` service, or the
host's credentials.

### How egress is enforced

1. Each sandbox gets its own `--internal` (host-only) network. There is no
   NAT: the VM physically cannot reach the internet.
2. sandboxd binds an HTTP proxy on that network's gateway
   (`10.213.N.1:3128`). It serves only the sandbox's own IP, so sandboxes
   can't use each other's proxies.
3. `HTTP(S)_PROXY` is set in every `exec`/`shell` and in `/etc/profile.d`.
   HTTPS uses `CONNECT host:port`; plain HTTP uses absolute-form requests.
   Each is checked against the allowlist and logged to
   `~/Library/Logs/Sandboxd/egress/<id>.jsonl`.
4. An allowlisted name that resolves to a loopback/private/link-local address
   is refused (`block_private_ips`), so DNS can't be used to aim at the host or LAN.
5. **DNS answers only allowlisted names.** sandboxd runs a small resolver on
   each gateway (`dns_port`, 3053). Allowlisted names get the addresses the
   host resolves, minus private ones. Everything else gets NXDOMAIN without
   being forwarded, so queries can't carry data out. This exists for tools
   that resolve a name before connecting (SSRF guards like pi-safe-search's
   `web_fetch`); the connection itself still has to go through the proxy.
6. **Install the pf anchor** (`sandbox firewall --install`). Without it,
   services on the host that listen on all interfaces (dev servers, AirPlay
   receiver on :5000/:7000, ...) can be reached from the sandbox through the
   gateway address over IPv4 and IPv6. The anchor drops all sandbox traffic to
   the host except the proxy and forward ports you open, and redirects port 53 to sandboxd's
   resolver (binding 53 needs root on macOS). Without the anchor, sandboxes
   have no DNS.

Tools must honor `HTTPS_PROXY` (curl, git over https, npm, pip, cargo, Claude
Code, and Node 24+ with `NODE_USE_ENV_PROXY=1` all do). SSH remotes won't
work: use HTTPS.

### Databases and other services

The proxy only speaks HTTP, and it refuses private addresses, so a sandbox
can't reach a Postgres on your Mac or a Redis on your LAN through it. Forward
the service instead:

```sh
sandbox create --forward localhost:5432          # Postgres on your Mac
sandbox create --forward 15100=10.0.1.20:6379    # Redis on the LAN, explicit port
sandbox forward 3fa2c1 localhost:3306            # add one to a running sandbox
sandbox unforward 3fa2c1 15306
sandbox firewall --open 15432                    # let sandboxes reach the port (sudo)
```

sandboxd listens on the sandbox's gateway and relays each connection to the
target, so any TCP client works. Inside the sandbox, connect to
`$SANDBOX_GATEWAY:<port>`. The port defaults to `15000 + port % 1000`
(5432 → 15432, 6379 → 15379) and must be within `forward_ports`
(15000–15999):

```sh
psql "postgres://me@$SANDBOX_GATEWAY:15432/app"
```

A forward is the one way to reach a private address, and it opens exactly
that host and port. Connections are logged as `FORWARD` in the egress log.
The gateway is in `NO_PROXY`, so HTTP clients reach forwarded services
directly.

With the pf anchor installed, a forward port is also closed at the firewall
until you open it with `sandbox firewall --open PORT` (which runs under
sudo). That's deliberate: neither an agent nor sandboxd can expose a port on
your Mac, and opening one is a separate, privileged step. `create` and
`sandbox forward` print the command when a port is still closed. Only ports
in `forward_ports` can be opened, and an opened port admits any sandbox, so
close it with `--close` when you're done.

### Workspaces

Inside a git repo, `sandbox create` runs
`treehouse get --lease --branch sandbox/<id>` and mounts the worktree at
`/workspace`. The repository's shared `.git` is mounted at the same absolute
path so the worktree's `gitdir:` pointer resolves and commits land on the
`sandbox/<id>` branch in your repo.

That `.git` is writable, which would let an agent plant something that runs
on the host the next time you use git. To prevent that, `hooks/` is mounted
read-only, and Apple's runtime can't bind-mount single files, so sandboxd
polls `.git/config` every 3 seconds. Any newly added execution-capable key
(`core.fsmonitor`, `core.hooksPath`, filter drivers, `!` aliases,
`includeIf`, ...) is reverted and logged. Other files the agent edits in the
worktree (Makefiles, package scripts, `.envrc`) are still code you run
yourself, so review them as you would any PR.

`sandbox destroy` refuses while the worktree has uncommitted changes (commit
them, or pass `--discard`). Otherwise it stops the VM, deletes the network,
and `treehouse return`s the worktree. The `sandbox/<id>` branch and its
commits stay in your repo. Outside a repo (or with `--scratch`) you get an empty
directory under `~/Library/Application Support/Sandboxd/workspaces/` that is
deleted on destroy.

### Agents

Any CLI agent works: `sandbox exec <id> -- <anything>`. **Agent presets**
(`~/Library/Application Support/Sandboxd/agents/<name>.toml`) package what an
agent needs so `-A <name>` sets it up. pi, claude, and codex presets are
installed on first run. Copy one to add another agent.

```toml
description = "pi coding agent"
command = ["pi"]                       # what `sandbox agent` / `--run` starts
profiles = []                          # allowlist profiles from policies/
allow = ["llm.example.com"]            # extra hosts
env = ["ANTHROPIC_API_KEY"]            # forwarded from your shell if set, never stored
setup = ["npm install -g some-plugin"] # run once at create, in /workspace
setup_allow = ["registry.npmjs.org"]   # reachable only while setup runs
forward = ["localhost:5432"]           # host services to relay in (see below)

[[config]]                             # host config copied in (not mounted)
host = "~/.pi/agent"
container = "/root/.pi/agent"
exclude = ["sessions", "bin"]          # or include = [...] for big dirs
sync_back = ["auth.json"]              # copied home on teardown if only the sandbox changed it
```

Config is **copied**, so an agent can't plant extensions, skills, or hooks
that later run on your Mac. Symlinks (e.g. skills linked from elsewhere) are
followed. `sync_back` lets refreshed OAuth tokens come home. If the host copy
changed too, the host wins.

The pi preset copies `~/.pi/agent` (minus sessions and the macOS `bin/`),
installs the `npm:` packages from your `settings.json` during setup, and
forwards provider API keys. Add your own provider hosts to its `allow` list.
To make pi the default, set `default_agents = ["pi"]` in `config.toml`
(`--no-agent` opts out).

Claude Code keeps its macOS login in the Keychain (not `~/.claude.json`),
so a sandbox needs its own credential:

- **Subscription (Pro/Max):** run `claude setup-token` and export the result
  as `CLAUDE_CODE_OAUTH_TOKEN`. It's a long-lived login token, not an API key:
  usage counts against your plan, and it can be revoked without touching your
  Mac's login.
- **API billing:** export a Console key as `ANTHROPIC_API_KEY`.

Copying the Keychain login in would break it:
refresh tokens rotate, so whichever side refreshed first would log the other
out. The claude preset writes a minimal `~/.claude.json` so first-run prompts
are skipped.

### Lifecycle and recovery

- **TTL**: sandboxes expire after `default_ttl` (8h) or `--ttl`. On expiry
  (or if the VM dies) the container is torn down. A dirty worktree is kept and
  the sandbox shows as `stopped` until you destroy it.
- **Restart**: on startup sandboxd re-attaches proxies to running sandboxes,
  tears down ones whose VMs are gone, and deletes any container or network
  labeled `dev.squirrelsoft.sandboxd` that it has no record of.
- **Daemon down**: proxies stop with it, so sandboxes fail closed.

## Files

`install.sh` puts `sandbox` and `sandboxd` in `~/.local/bin` and loads
`~/Library/LaunchAgents/dev.squirrelsoft.sandboxd.plist`.

```
~/Library/Application Support/Sandboxd/
    config.toml        image, cpus, memory, default_ttl, global allowlist, subnets, ports
    policies/*.toml    allowlist profiles (claude, codex, github, npm, pypi, crates)
    agents/*.toml      agent presets (pi, claude, codex)
    state.db           sandboxes + lifecycle events
    sandboxd.sock      0600, owner-only; peer uid is checked too
    workspaces/        scratch workspaces
    git-snapshots/     per-sandbox .git/config baselines for the guard
~/Library/Logs/Sandboxd/
    sandboxd.log
    egress/<id>.jsonl  every proxied connection: open/close/deny/error
```

## CLI

| Command | |
| --- | --- |
| `sandbox create [-A AGENT]… [--no-agent] [--run \| --shell] [-p PROFILE]… [-a HOST]… [-e KEY[=VAL]]… [--forward [PORT=]HOST:PORT]… [--repo P \| --scratch] [-b BRANCH] [--base REF] [--ttl 2h] [--image I] [--cpus N] [--memory 8G] [-n NAME] [--rm] [-q]` | Create a sandbox |
| `sandbox agent [ID] [-A NAME] [-- ARGS…]` | Start the sandbox's agent (interactive) |
| `sandbox shell [ID]` | Interactive login shell in `/workspace` |
| `sandbox exec [ID] -- CMD…` | Run a command, exit with its status |
| `sandbox allow ID HOST…` / `sandbox deny ID HOST…` | Change the allowlist live |
| `sandbox forward ID [[PORT=]HOST:PORT…]` / `sandbox unforward ID PORT…` | List, add, or remove port forwards live |
| `sandbox firewall [--open PORT]… [--close PORT]… [--install \| --uninstall]` | Host firewall status and changes (sudo) |
| `sandbox logs [ID] [-f] [--denied] [--json]` | Egress log |
| `sandbox ls`, `sandbox info [ID]` | Inspect |
| `sandbox destroy [ID] [--discard]` | Tear down, return worktree |
| `sandbox agents`, `sandbox profiles`, `sandbox status` | |

`ID` can be an id, a unique id prefix, or `--name`. Leave it out and the CLI
picks the sandbox whose workspace contains your current directory, or the only
sandbox there is.

Allowlist entries: `example.com` (ports 80/443), `example.com:8443`,
`*.example.com` (subdomains only), `.example.com` (domain + subdomains), `*`
(everything, for debugging).

**Secrets**: `-e KEY` (no value) stores only the name. The CLI forwards the
value from your environment on each `exec`/`shell`, so API keys never land in
`state.db`. `-e KEY=VAL` is stored and set on the container itself.

## Development

```sh
cargo test
# isolated instance: state in .dev, separate socket
export SANDBOXD_HOME=$PWD/.dev/home SANDBOXD_SOCKET=$PWD/.dev/sandboxd.sock
cargo run -p sandboxd &
cargo run -p sandbox -- create --scratch --image alpine:3.20 -a dl-cdn.alpinelinux.org
```

`SANDBOXD_LOG=debug` logs every `container` invocation.

## Known limits

- A sandbox is a single container. For databases and other services, use
  `--forward` to a service on your Mac or LAN, or install it in the image.
- `sandbox deny` affects new connections only; open tunnels run to completion.
- The git guard polls, so there is a window of up to 3s before a planted
  config entry is reverted. Don't run git in the repo on the host while an
  untrusted agent is active if that window matters to you.
- Agents run as root inside their VM. The VM is the isolation boundary.
- Force-deleting a running container (outside sandboxd) can leave its network
  undeletable until `container system stop && container system start`.

## License

MIT. Copyright (c) 2026 SquirrelSoft LLC. See [LICENSE](LICENSE).
