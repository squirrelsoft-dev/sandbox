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

## Install

Requirements: Apple `container` ≥ 0.10 (`container system start`), treehouse,
Rust.

```sh
container build -t sandbox-agent:latest images/agent   # default image (pi, claude, codex)
scripts/install.sh                                     # binaries + launchd agent
sandbox firewall --install                             # pf anchor (recommended; sudo)
```

If the build can't resolve hosts (common with Tailscale or other VPN DNS on
the Mac), give the builder a public resolver and retry:
`container builder delete; container builder start --dns 1.1.1.1`.
This only affects the builder; sandboxes resolve through sandboxd.

`install.sh` puts `sandbox` and `sandboxd` in `~/.local/bin` and loads
`~/Library/LaunchAgents/dev.squirrelsoft.sandboxd.plist`. Logs go to
`~/Library/Logs/Sandboxd/sandboxd.log`.

Files:

```
~/Library/Application Support/Sandboxd/
    config.toml        image, cpus, memory, default_ttl, global allowlist, subnets, ports
    policies/*.toml    allowlist profiles (claude, codex, github, npm, pypi, crates)
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
