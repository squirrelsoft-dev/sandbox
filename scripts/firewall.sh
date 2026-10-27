#!/bin/bash
# Fence sandbox networks off from the macOS host with a pf anchor.
#
# Sandbox networks are host-only, so they have no route to the internet, but
# the host itself is reachable on the gateway address: any service listening
# on 0.0.0.0 / :: (dev servers, AirPlay receiver, sshd, ...) would be open to
# agents. These rules drop everything from sandbox subnets to the host except
# the egress proxy port and any forward ports you open explicitly, and
# redirect DNS (port 53, which sandboxd can't bind without root) to sandboxd's
# allowlist-scoped resolver.
#
# Rules load into a sub-anchor of Apple's stock `com.apple/*` anchor, so
# /etc/pf.conf is never edited. A LaunchDaemon reloads them at boot.
#
#   sudo scripts/firewall.sh install         # write anchor + boot job, load now
#   sudo scripts/firewall.sh uninstall
#   sudo scripts/firewall.sh open 15432      # let sandboxes reach a --forward port
#   sudo scripts/firewall.sh close 15432
#   scripts/firewall.sh status               # sudo shows the live rules too
#
# `sandbox firewall` runs this same script (embedded in the CLI) with sudo.
#
# Keep these in sync with subnet_prefix_v4 / subnet_prefix_v6 / proxy_port /
# dns_port / forward_ports in ~/Library/Application Support/Sandboxd/config.toml.
set -euo pipefail

V4_PREFIX="${SANDBOX_SUBNET_V4:-10.213}"
V6_PREFIX="${SANDBOX_SUBNET_V6:-fd5b:d0c5:a17e}"
PROXY_PORT="${SANDBOX_PROXY_PORT:-3128}"
DNS_PORT="${SANDBOX_DNS_PORT:-3053}"
FORWARD_PORTS="${SANDBOX_FORWARD_PORTS:-15000-15999}"

LABEL="dev.squirrelsoft.sandboxd.pf"
ANCHOR="com.apple/250.dev.squirrelsoft.sandboxd"
RULES="/etc/pf.anchors/dev.squirrelsoft.sandboxd"
PLIST="/Library/LaunchDaemons/${LABEL}.plist"
# Forward ports opened with `open`, one per line. Root-owned, so neither an
# agent nor sandboxd can open a port; it takes sudo.
PORTS_FILE="/etc/pf.anchors/dev.squirrelsoft.sandboxd.ports"

need_root() {
  if [[ $EUID -ne 0 ]]; then
    echo "run with sudo" >&2
    exit 1
  fi
}

write_rules() {
  # vmnet names its bridges bridge100, bridge101, ... Link-local IPv6 can't be
  # matched by source prefix, so block it per bridge interface instead. This
  # also stops link-local IPv6 TCP/UDP from other vmnet guests (rarely used).
  local bridges rdr open forward_rule=""
  bridges=$(printf 'bridge%d ' $(seq 100 163))
  # One redirect per bridge: the target is that bridge's own (gateway) address.
  rdr=$(for i in $(seq 100 163); do
    printf 'rdr pass on bridge%d inet proto udp from $sandbox4 to $sandbox4 port 53 -> (bridge%d:0) port %s\n' "$i" "$i" "$DNS_PORT"
  done)
  open=$(open_ports | tr '\n' ' ')
  if [[ -n "$open" ]]; then
    forward_rule="pass in quick inet proto tcp from \$sandbox4 to \$sandbox4 port { ${open}}"
  fi
  cat >"$RULES" <<EOF
# Managed by sandboxd scripts/firewall.sh. Do not edit by hand.
sandbox4 = "${V4_PREFIX}.0.0/16"
sandbox6 = "${V6_PREFIX}::/48"
bridges = "{ ${bridges}}"

# DNS: sandboxd answers allowlisted names on dns_port.
${rdr}

# The per-sandbox egress proxy on each gateway address.
pass in quick inet proto tcp from \$sandbox4 to \$sandbox4 port ${PROXY_PORT}

# Forward ports opened with \`sandbox firewall --open PORT\`.
${forward_rule:-# (none open)}

# Everything else from a sandbox is dropped. Host-only networks can only
# reach the host, so "to any" means "to the host"; \`self\` would be frozen to
# the addresses present at load time and miss bridges created later.
block drop in quick inet from \$sandbox4 to any
block drop in quick inet6 from \$sandbox6 to any
block drop in quick on \$bridges inet6 proto { tcp udp } from fe80::/10 to any
EOF
  chmod 644 "$RULES"
}

open_ports() {
  [[ -f "$PORTS_FILE" ]] && grep -E '^[0-9]+$' "$PORTS_FILE" | sort -un
  return 0
}

# Validate forward ports: numeric and inside forward_ports, so `open 22`
# can't expose sshd.
check_ports() {
  local lo=${FORWARD_PORTS%-*} hi=${FORWARD_PORTS#*-} p
  [[ $# -gt 0 ]] || { echo "name at least one port" >&2; exit 2; }
  for p in "$@"; do
    if ! [[ "$p" =~ ^[0-9]+$ ]] || (( p < lo || p > hi )); then
      echo "port $p is not in the forward range ${FORWARD_PORTS}" >&2
      exit 2
    fi
  done
}

need_installed() {
  if [[ ! -f "$PLIST" ]]; then
    echo "the sandbox firewall isn't installed; run install first" >&2
    exit 1
  fi
}

reload() {
  write_rules
  pfctl -nf "$RULES" -a "$ANCHOR" 2>/dev/null
  pfctl -a "$ANCHOR" -f "$RULES" 2>/dev/null
}

write_plist() {
  cat >"$PLIST" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>${LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>/bin/sh</string>
    <string>-c</string>
    <string>/sbin/pfctl -E; /sbin/pfctl -a ${ANCHOR} -f ${RULES}</string>
  </array>
  <key>RunAtLoad</key><true/>
</dict>
</plist>
EOF
  chmod 644 "$PLIST"
}

case "${1:-}" in
  install)
    need_root
    write_rules
    pfctl -nf "$RULES" -a "$ANCHOR"   # syntax check before touching anything live
    write_plist
    launchctl bootout system "$PLIST" 2>/dev/null || true
    launchctl bootstrap system "$PLIST"
    pfctl -a "$ANCHOR" -f "$RULES" 2>/dev/null
    pfctl -E 2>/dev/null || true
    echo "Installed. Active rules:"
    pfctl -a "$ANCHOR" -sr
    ;;
  uninstall)
    need_root
    launchctl bootout system "$PLIST" 2>/dev/null || true
    pfctl -a "$ANCHOR" -F rules 2>/dev/null || true
    rm -f "$PLIST" "$RULES" "$PORTS_FILE"
    echo "Removed."
    ;;
  open|close)
    need_root
    need_installed
    cmd=$1
    shift
    check_ports "$@"
    current=$(open_ports)
    if [[ $cmd == open ]]; then
      printf '%s\n' $current "$@" | sort -un >"$PORTS_FILE"
    else
      printf '%s\n' $current | grep -vxF -f <(printf '%s\n' "$@") >"$PORTS_FILE" || true
    fi
    chmod 644 "$PORTS_FILE"
    reload
    now=$(open_ports | xargs)
    echo "Open forward ports: ${now:-none}"
    ;;
  status)
    if [[ -f "$PLIST" ]]; then echo "Installed."; else echo "Not installed."; fi
    now=$(open_ports | xargs)
    echo "Open forward ports: ${now:-none} (range ${FORWARD_PORTS})"
    if [[ $EUID -eq 0 ]]; then
      pfctl -s info 2>/dev/null | head -1
      echo "Anchor ${ANCHOR}:"
      pfctl -a "$ANCHOR" -sr 2>/dev/null || echo "  (not loaded)"
    fi
    ;;
  *)
    echo "usage: sudo $0 install|uninstall|open PORT...|close PORT...|status" >&2
    exit 2
    ;;
esac
