#!/bin/bash
# Build sandboxd + sandbox, install them to ~/.local/bin, and register sandboxd
# as a per-user launchd agent that starts at login and restarts if it dies.
#
#   scripts/install.sh            # install or upgrade
#   scripts/install.sh uninstall  # stop and remove the agent (keeps state)
set -euo pipefail

LABEL="dev.squirrelsoft.sandboxd"
BIN_DIR="$HOME/.local/bin"
PLIST="$HOME/Library/LaunchAgents/${LABEL}.plist"
LOG_DIR="$HOME/Library/Logs/Sandboxd"
DOMAIN="gui/$(id -u)"
cd "$(dirname "$0")/.."

if [[ "${1:-}" == "uninstall" ]]; then
  launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
  rm -f "$PLIST"
  echo "sandboxd agent removed. Binaries in $BIN_DIR and state in ~/Library/Application Support/Sandboxd were left alone."
  exit 0
fi

cargo build --release
mkdir -p "$BIN_DIR" "$LOG_DIR" "$(dirname "$PLIST")"
install -m 755 target/release/sandboxd target/release/sandbox "$BIN_DIR/"

# launchd starts agents with a bare PATH; sandboxd shells out to container,
# treehouse, and git.
AGENT_PATH="$BIN_DIR:/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin"

sed -e "s|@BIN@|$BIN_DIR/sandboxd|" \
    -e "s|@PATH@|$AGENT_PATH|" \
    -e "s|@HOME@|$HOME|" \
    -e "s|@LOGS@|$LOG_DIR|" \
    launchd/${LABEL}.plist >"$PLIST"

# bootout returns before the old job is gone; bootstrapping too soon fails
# with "5: Input/output error". Wait for it, then retry briefly to be safe.
launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null || true
for _ in $(seq 50); do
  launchctl print "$DOMAIN/$LABEL" >/dev/null 2>&1 || break
  sleep 0.2
done
for attempt in 1 2 3 4 5; do
  launchctl bootstrap "$DOMAIN" "$PLIST" && break
  [[ $attempt == 5 ]] && exit 1
  sleep 1
done
sleep 1
"$BIN_DIR/sandbox" status
