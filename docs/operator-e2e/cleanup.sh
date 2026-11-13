#!/usr/bin/env bash
# Remove the token, the profiles files and the throwaway directories.
# Stop the server first (Ctrl-C in its terminal) and remove_pane every pane.
set -u
E2E_HOME="${E2E_HOME:-$HOME/.config/shepherd-e2e}"
WORK="${E2E_WORK:-/tmp/shepherd-e2e}"
case "$WORK" in /tmp/shepherd-e2e*) ;; *) echo "refusing to remove $WORK" >&2; exit 1 ;; esac
case "$E2E_HOME" in */shepherd-e2e) ;; *) echo "refusing to remove $E2E_HOME" >&2; exit 1 ;; esac
rm -rf "$WORK"
rm -f "$E2E_HOME/token" "$E2E_HOME/profiles.json" "$E2E_HOME/bad-profiles.json"
rmdir "$E2E_HOME" 2>/dev/null
echo "removed $WORK and the files in $E2E_HOME"
if lsof -nP -iTCP:"${E2E_PORT:-8765}" -sTCP:LISTEN >/dev/null 2>&1; then
  echo "WARNING: something still listens on port ${E2E_PORT:-8765}" >&2
fi
