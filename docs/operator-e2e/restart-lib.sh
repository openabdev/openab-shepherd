#!/usr/bin/env bash
# Shared helpers for the restart-survival scripts (restart-survival.md). Source it:
#
#   HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
#   source "$HERE/restart-lib.sh"
#
# It starts a real `openab-shepherd hold` with a throwaway token, a throwaway profiles
# file and a throwaway port, and talks to it through the same MCP helpers as the other
# operator scripts (mcp.sh). RUN IT YOURSELF: agents never start the Shepherd server.
# Nothing here calls a model: the panes are `sh -c` scripts that print OSC 7501 status
# reports and a recorded Kiro permission dialog, and the verdicts come from MCP fields,
# files the panes write, process ids and the holder log.
#
# Works on macOS and Linux: no `stat -f`, no lsof. Needs bash, python3,
# openssl, curl, pgrep.
#
# Settings (environment):
#   SHEPHERD_BIN  the binary (default ~/.local/bin/openab-shepherd)
#   PORT          loopback port (default 8766, so it does not clash with the other runbook)
#   R_HOME        token and profiles (default ~/.config/shepherd-e2e-restart)
#   R_WORK        working directories and the holder log (default /tmp/shepherd-e2e-restart)
#   KEEP=1        leave the holder and the files behind when a script ends

R_HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
R_HOME="${R_HOME:-$HOME/.config/shepherd-e2e-restart}"
R_WORK="${R_WORK:-/tmp/shepherd-e2e-restart}"
BIN="${SHEPHERD_BIN:-$HOME/.local/bin/openab-shepherd}"
PORT="${PORT:-8766}"
R_VERSION_FILE="$R_WORK/holder-version"
R_LOG="$R_WORK/holder.log"
R_PIDFILE="$R_WORK/holder.pid"
R_FAILS=0
R_PASSES=0

export SHEPHERD_URL="http://127.0.0.1:$PORT/mcp"
export SHEPHERD_TOKEN_FILE="$R_HOME/token"
export SHEPHERD_WORK="$R_WORK"
# shellcheck source=mcp.sh
source "$R_HERE/mcp.sh"

r_die() { echo "restart: $*" >&2; exit 1; }
r_say() { echo "restart: $*"; }

# r_mode PATH -> the octal permission bits (GNU and BSD stat differ).
r_mode() {
  if [ "$(uname -s)" = Darwin ]; then stat -f '%Lp' "$1"; else stat -c '%a' "$1"; fi
}

# r_port_in_use PORT -> 0 when something accepts connections there.
r_port_in_use() {
  python3 -c 'import socket,sys
s = socket.socket(); s.settimeout(1)
sys.exit(0 if s.connect_ex(("127.0.0.1", int(sys.argv[1]))) == 0 else 1)' "$1"
}

# ---------------------------------------------------------------------------
# Verdicts
# ---------------------------------------------------------------------------

# r_check DESCRIPTION COMMAND... : PASS when the command succeeds.
r_check() {
  local what="$1"; shift
  if "$@"; then
    echo "PASS $what"; R_PASSES=$((R_PASSES + 1))
  else
    echo "FAIL $what"; R_FAILS=$((R_FAILS + 1))
  fi
}

# r_finish : print the summary the operator pastes into the issue; exit 1 on any FAIL.
r_finish() {
  echo
  echo "host:    $(uname -srm)"
  echo "binary:  $("$BIN" --help 2>&1 | head -1) ($BIN)"
  echo "summary: $R_PASSES passed, $R_FAILS failed"
  if [ "$R_FAILS" -eq 0 ]; then echo "RESULT: PASS"; else echo "RESULT: FAIL (see $R_LOG)"; fi
  [ "$R_FAILS" -eq 0 ]
}

# ---------------------------------------------------------------------------
# Setup: token, profiles, working directories
# ---------------------------------------------------------------------------

# r_setup : create the token, the profiles file and the pane working directories.
# Safe to run again: it replaces the working directories and the profiles.
r_setup() {
  [ -x "$BIN" ] || r_die "no executable at $BIN (build it on the build host and copy it, see restart-survival.md)"
  for tool in python3 openssl curl pgrep; do
    command -v "$tool" >/dev/null || r_die "$tool is not installed"
  done
  case "$R_WORK" in */shepherd-e2e-restart*) ;; *) r_die "refusing to use R_WORK=$R_WORK (the path must contain shepherd-e2e-restart)" ;; esac
  case "$R_HOME" in */shepherd-e2e-restart*) ;; *) r_die "refusing to use R_HOME=$R_HOME (the path must contain shepherd-e2e-restart)" ;; esac
  if git -C "$R_HOME" rev-parse --show-toplevel >/dev/null 2>&1; then
    r_die "$R_HOME is inside a git repository; the token and profiles must be outside every repository"
  fi
  (
    umask 077
    mkdir -p "$R_HOME" "$R_WORK"
    chmod 700 "$R_HOME" "$R_WORK"
    [ -s "$R_HOME/token" ] || openssl rand -hex 32 > "$R_HOME/token"
    chmod 600 "$R_HOME/token"
    for d in server turns dialog-live dialog-wrapped wrapped-idle wrapped-ticker routine-fresh routine-giveup; do
      rm -rf "${R_WORK:?}/$d"; mkdir -p "$R_WORK/$d"
    done
    R_WORK="$R_WORK" R_HOME="$R_HOME" python3 - <<'PY'
import json, os

work, home = os.environ["R_WORK"], os.environ["R_HOME"]
WORKING = r"\033]7501;state=working\007"
DONE = r"\033]7501;state=done\007"
BLOCKED = r"\033]7501;state=blocked:kind=permission\007"
# A recorded kiro-cli 3.1.0 permission dialog (the same text src/dialog.rs parses in its tests).
DIALOG = """  Use the shell tool to run exactly this command in the current directory and nothing else: touch shepherd_marker.txt

\u2193 Shell touch shepherd_marker.txt
    \u2570 description=Create an empty marker file, cwd=/tmp/shepherd-e2e-restart, run_in_background=false

\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500
 Create an empty marker file requires approval
 shell \u2192 touch shepherd_marker.txt

 \u276f Allow once
   Deny once
   Create allow rule to approve future similar actions
   Create deny rule to block future similar actions
\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500\u2500
 esc to close \u00b7 \u2191\u2193 to navigate \u00b7 \u21b5 to select \u00b7 Tab to edit
"""
# More than the holder's 1 MiB tail, in 100-column lines.
FILLER = "head -c 1400000 /dev/zero | tr '\\000' x | fold -w 100; "
SLEEP = "exec sleep 31337"          # a unique duration, so a leftover pane is easy to find


def profile(name, script, routine=None):
    d = os.path.join(work, name)
    p = {"name": name, "agent": "kiro", "argv": ["/bin/sh", "-c", script], "cwds": [d]}
    if routine:
        p["routine"] = routine
    with open(os.path.join(d, "dialog.txt"), "w") as f:
        f.write(DIALOG)
    return p


dialog = (f"printf '{WORKING}'; printf '{BLOCKED}'; cat dialog.txt; "
          f"IFS= read -r l; echo got > received.txt; {SLEEP}")
nudged = {"idle_timeout_secs": 10, "cooldown_secs": 1, "nudge_text": "continue the work"}
profiles = [
    # Two finished turns, then nothing.
    profile("turns", f"printf '{WORKING}'; printf '{DONE}'; printf '{WORKING}'; printf '{DONE}'; "
                     f"echo ready; {SLEEP}"),
    # A permission dialog on a short tail, and one behind more than 1 MiB of output.
    profile("dialog-live", dialog),
    profile("dialog-wrapped", FILLER + dialog),
    # A wrapped tail and then silence (stays replayed), and one that keeps printing.
    profile("wrapped-idle", FILLER + f"printf '{WORKING}'; printf '{DONE}'; {SLEEP}"),
    profile("wrapped-ticker", FILLER + f"printf '{WORKING}'; printf '{DONE}'; "
                                       "while :; do echo tick; sleep 2; done"),
    # The routine: one pane that has not been nudged yet, one that has used its only nudge.
    profile("routine-fresh", f"printf '{WORKING}'; printf '{DONE}'; cat >> typed.txt",
            dict(nudged, max_consecutive_nudges=2)),
    profile("routine-giveup", f"printf '{WORKING}'; printf '{DONE}'; cat >> typed.txt",
            dict(nudged, max_consecutive_nudges=1)),
]
with open(os.path.join(home, "profiles.json"), "w") as f:
    json.dump({"allow_arbitrary_command": False, "profiles": profiles}, f, indent=2)
    f.write("\n")
os.chmod(os.path.join(home, "profiles.json"), 0o600)
PY
  ) || r_die "setup failed"
  : > "$R_VERSION_FILE"
  r_say "setup done: token and profiles in $R_HOME, panes in $R_WORK"
}

# ---------------------------------------------------------------------------
# The holder
# ---------------------------------------------------------------------------

# r_start_holder : start `openab-shepherd hold` in the background and wait for MCP.
# The version file ($R_VERSION_FILE) is read by every front at its start (see the
# version-mismatch script); it is empty here, which means the real protocol number.
r_start_holder() {
  [ -f "$R_HOME/profiles.json" ] || r_die "run setup first (holder-start.sh does)"
  if r_port_in_use "$PORT"; then r_die "port $PORT is in use; set PORT=<another port>"; fi
  : > "$R_LOG"
  (
    cd "$R_WORK/server" || exit 1
    exec env -u OPENAB_SHEPHERD_BACKEND -u OPENAB_SHEPHERD_TOKEN \
      OPENAB_SHEPHERD_TOKEN_FILE="$R_HOME/token" \
      OPENAB_SHEPHERD_PROFILES="$R_HOME/profiles.json" \
      OPENAB_SHEPHERD_TEST_HOLDER_VERSION="@$R_VERSION_FILE" \
      "$BIN" hold -- --host 127.0.0.1 --port "$PORT"
  ) >> "$R_LOG" 2>&1 < /dev/null &
  echo $! > "$R_PIDFILE"
  r_wait_mcp 30 || { tail -20 "$R_LOG" >&2; r_die "the server did not come up within 30 s"; }
  r_say "holder pid $(r_holder_pid), front pid $(r_front_pid), port $PORT, log $R_LOG"
}

r_holder_pid() {
  local pid
  pid="$(cat "$R_PIDFILE" 2>/dev/null)" || return 1
  kill -0 "$pid" 2>/dev/null || return 1
  echo "$pid"
}

# r_front_pid [HOLDER_PID] -> the pid of the holder's current front, if one runs.
r_front_pid() {
  local holder="${1:-$(r_holder_pid)}"
  [ -n "$holder" ] || return 1
  pgrep -P "$holder" -f 'front --ops-fd' | head -1
}

# r_pane_pid [HOLDER_PID] -> the leader of the pane that runs `sleep 31337`.
r_pane_pid() {
  local holder="${1:-$(r_holder_pid)}"
  [ -n "$holder" ] || return 1
  pgrep -P "$holder" -f 'sleep 31337' | head -1
}

# r_mcp_up -> 0 when an authenticated tools/call is answered.
r_mcp_up() { list_panes >/dev/null 2>&1; }

# r_wait_mcp SECONDS
r_wait_mcp() {
  local end=$((SECONDS + $1))
  while [ "$SECONDS" -lt "$end" ]; do
    r_mcp_up && return 0
    sleep 0.3
  done
  return 1
}

# r_wait_new_front OLD_FRONT_PID SECONDS -> 0 once another front serves MCP.
r_wait_new_front() {
  local old="$1" end=$((SECONDS + $2)) now
  while [ "$SECONDS" -lt "$end" ]; do
    now="$(r_front_pid)"
    if [ -n "$now" ] && [ "$now" != "$old" ] && r_mcp_up; then return 0; fi
    sleep 0.2
  done
  return 1
}

# r_log_has REGEX [MIN_COUNT]
r_log_has() {
  local n
  n="$(grep -Ec -- "$1" "$R_LOG" 2>/dev/null)" || n=0
  [ "$n" -ge "${2:-1}" ]
}

# r_stop_holder : SIGTERM, which ends every pane; SIGKILL after 15 s.
r_stop_holder() {
  local pid
  pid="$(r_holder_pid)" || return 0
  kill -TERM "$pid" 2>/dev/null
  local end=$((SECONDS + 15))
  while kill -0 "$pid" 2>/dev/null && [ "$SECONDS" -lt "$end" ]; do sleep 0.2; done
  kill -0 "$pid" 2>/dev/null && kill -KILL "$pid" 2>/dev/null
  return 0
}

# r_cleanup : stop the holder and remove the throwaway files (not with KEEP=1).
r_cleanup() {
  if [ "${KEEP:-}" = 1 ]; then r_say "KEEP=1: leaving the holder and $R_WORK, $R_HOME"; return 0; fi
  r_stop_holder
  case "$R_WORK" in */shepherd-e2e-restart*) rm -rf "$R_WORK" ;; esac
  case "$R_HOME" in */shepherd-e2e-restart*) rm -rf "$R_HOME" ;; esac
  if pgrep -f 'sleep 31337' >/dev/null 2>&1; then
    echo "WARNING: a pane's sleep 31337 is still running (kill it by hand: pkill -f 'sleep 31337')" >&2
  fi
}

# r_begin : the start of every scenario script: setup, a holder, cleanup on exit.
r_begin() {
  r_setup
  trap r_cleanup EXIT
  r_start_holder
}

# ---------------------------------------------------------------------------
# Pane helpers (through the MCP helpers of mcp.sh)
# ---------------------------------------------------------------------------

# r_start NAME PROFILE : start the pane NAME from the profile of the same directory.
r_start() { start_pane "$2" "$R_WORK/$2" "$1" >/dev/null; }

# r_field NAME PATH -> a field of get_status (dotted path; "null" when absent).
r_field() {
  get_status "$1" 2>/dev/null | python3 -c 'import json, sys
cur = json.load(sys.stdin)
for part in sys.argv[1].split("."):
    cur = cur.get(part) if isinstance(cur, dict) else None
print("null" if cur is None else (cur if isinstance(cur, str) else json.dumps(cur)))' "$2" 2>/dev/null
}

# r_wait_field NAME PATH VALUE SECONDS
r_wait_field() {
  local end=$((SECONDS + $4))
  while [ "$SECONDS" -lt "$end" ]; do
    [ "$(r_field "$1" "$2")" = "$3" ] && return 0
    sleep 0.5
  done
  return 1
}

# r_error CMD... -> the `error` code of a failing tool call (empty when it succeeded).
r_error() {
  "$@" 2>/dev/null | python3 -c 'import json, sys
try:
    print(json.load(sys.stdin).get("error", ""))
except Exception:
    print("")'
}

# r_file_lines FILE -> the number of lines (0 when the file is missing).
r_file_lines() { if [ -f "$1" ]; then wc -l < "$1" | tr -d ' '; else echo 0; fi; }

# r_wait_file PATH SECONDS
r_wait_file() {
  local end=$((SECONDS + $2))
  while [ "$SECONDS" -lt "$end" ]; do
    [ -e "$1" ] && return 0
    sleep 0.3
  done
  return 1
}

# r_now_ms -> the time in milliseconds (BSD date has no %N).
r_now_ms() { python3 -c 'import time; print(int(time.time() * 1000))'; }

# r_defunct_children HOLDER_PID -> lines for zombie children of the holder (none is good).
r_defunct_children() {
  ps -axo ppid=,stat=,comm= | awk -v h="$1" '$1 == h && $2 ~ /^Z/'
}

# r_quiet CMD... : run a command with its output discarded (for use inside r_check).
r_quiet() { "$@" >/dev/null 2>&1; }

# r_wait_new_front_of HOLDER_PID OLD_FRONT_PID SECONDS : like r_wait_new_front, for a holder
# that a service manager started (there is no pid file).
r_wait_new_front_of() {
  local holder="$1" old="$2" end=$((SECONDS + $3)) now
  while [ "$SECONDS" -lt "$end" ]; do
    now="$(r_front_pid "$holder")"
    if [ -n "$now" ] && [ "$now" != "$old" ] && r_mcp_up; then return 0; fi
    sleep 0.2
  done
  return 1
}

# r_pane_gone PID -> 0 when no process with that pid still runs `sleep 31337`.
r_pane_gone() {
  ! ps -p "$1" -o args= 2>/dev/null | grep -q 'sleep 31337'
}
