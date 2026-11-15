#!/usr/bin/env bash
# macOS (macmini): run the holder as a launchd agent and check both kinds of restart.
#   launchctl kill USR2   restarts the server only: the pane survives.
#   launchctl kickstart -k restarts the holder: the pane ends, as documented.
# It installs a throwaway agent (dev.openab.shepherd-e2e-restart) in ~/Library/LaunchAgents
# and removes it when it ends. Run it yourself, from a logged-in Terminal session; agents
# never start the server. Calls no model.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=restart-lib.sh
source "$HERE/restart-lib.sh"

[ "$(uname -s)" = Darwin ] || r_die "this script is for macOS; use service-restart-linux.sh on Linux"
LABEL="dev.openab.shepherd-e2e-restart"
PLIST="$HOME/Library/LaunchAgents/$LABEL.plist"
DOMAIN="gui/$(id -u)"

service_cleanup() {
  launchctl bootout "$DOMAIN/$LABEL" 2>/dev/null
  rm -f "$PLIST"
  r_cleanup
}

# The job's pid, from `launchctl print`.
job_pid() {
  launchctl print "$DOMAIN/$LABEL" 2>/dev/null | awk '$1 == "pid" && $2 == "=" { print $3; exit }'
}

r_setup
trap service_cleanup EXIT
: > "$R_LOG"
mkdir -p "$HOME/Library/LaunchAgents"
cat > "$PLIST" <<PLISTEOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$LABEL</string>
  <key>ProgramArguments</key>
  <array>
    <string>$BIN</string><string>hold</string><string>--</string>
    <string>--host</string><string>127.0.0.1</string><string>--port</string><string>$PORT</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>OPENAB_SHEPHERD_TOKEN_FILE</key><string>$R_HOME/token</string>
    <key>OPENAB_SHEPHERD_PROFILES</key><string>$R_HOME/profiles.json</string>
    <key>PATH</key><string>/usr/bin:/bin:/usr/sbin:/sbin</string>
  </dict>
  <key>WorkingDirectory</key><string>$R_WORK/server</string>
  <key>StandardErrorPath</key><string>$R_LOG</string>
  <key>StandardOutPath</key><string>$R_LOG</string>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><false/>
</dict>
</plist>
PLISTEOF
r_port_in_use "$PORT" && r_die "port $PORT is in use; set PORT=<another port>"
launchctl bootstrap "$DOMAIN" "$PLIST" || r_die "launchctl bootstrap failed"
r_wait_mcp 30 || { tail -20 "$R_LOG" >&2; r_die "the server did not come up within 30 s"; }

holder="$(job_pid)"
r_check "launchd runs the holder (pid ${holder:-none})" test -n "$holder"
r_start t1 turns
r_check "the pane reaches done with 2 turns" r_wait_field t1 turn 2 20
pane_before="$(r_pane_pid "$holder")"
front="$(r_front_pid "$holder")"

launchctl kill USR2 "$DOMAIN/$LABEL"
r_check "launchctl kill USR2: a new front serves MCP" r_wait_new_front_of "$holder" "$front" 30
r_check "launchctl kill USR2: the holder is the same process" test "$(job_pid)" = "$holder"
r_check "launchctl kill USR2: the pane's program is the same process" test "$(r_pane_pid "$holder")" = "$pane_before"
r_check "launchctl kill USR2: turn is still 2" test "$(r_field t1 turn)" = 2

launchctl kickstart -k "$DOMAIN/$LABEL"
end=$((SECONDS + 30))
while [ "$SECONDS" -lt "$end" ]; do
  now="$(job_pid)"
  [ -n "$now" ] && [ "$now" != "$holder" ] && break
  sleep 0.5
done
r_check "kickstart -k: launchd runs a new holder (was $holder, now ${now:-none})" test -n "${now:-}" -a "${now:-}" != "$holder"
r_wait_mcp 30
r_check "kickstart -k: the old pane's program ended" r_pane_gone "$pane_before"
r_check "kickstart -k: the pane is gone (not_found)" test "$(r_error get_status t1)" = not_found
r_finish
