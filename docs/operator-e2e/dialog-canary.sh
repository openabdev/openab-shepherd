#!/usr/bin/env bash
# Dialog drift canary (issue #65): does the permission dialog of the agent CLI you have installed
# still parse with the rule Shepherd ships? One agent per run, ONE model prompt per run.
#
#   bash dialog-canary.sh kiro|devin|opencode
#
# It starts a real `openab-shepherd hold` on loopback with a throwaway token, a clients file
# that grants only observe, read, control, start, dialog and remove, and one launch profile for
# the agent. It sends one prompt that makes the agent try `touch <marker>` through its shell
# tool, waits (bounded) for `blocked` with kind `permission`, reads the screen, and compares it
# STRUCTURALLY with the recorded fixture (never byte for byte; see dialog-canary-check.py). Then
# it answers `deny_once` through the production parser, requires success, and requires that the
# marker file does NOT exist. A refusal from `answer_dialog` is the drift: the script prints the
# CLI version and the end of the screen (`seen`) as the report. It always removes the pane and
# deletes its throwaway directories.
# The checks are listed in dialog-canary.md. Takes about 2 minutes. RUN IT YOURSELF: agents never
# start the Shepherd server, and never run this script.
#
#   Step  Where
#   0     c_prereq: the CLI, its login, the pinned version (a notice, not a failure); SKIP when the
#         agent cannot authenticate (counted in the summary; nothing is started, no model is called)
#   1     c_setup, c_start_holder: token, clients file, profile, throwaway home and directory
#   2     start_pane, the first screen: no login prompt (a login prompt aborts the pane; SKIP)
#   3     send_input (the one prompt), wait_status: blocked with kind permission
#   4     read_output, dialog-canary-check.py: the structure of the dialog
#   5     answer_dialog deny_once: success; the marker file is absent
#   6     remove_pane
#
# The script never runs `kiro-cli login` or any login: a missing login is a SKIP. For Kiro it
# checks only that KIRO_API_KEY is set (the value is never printed or put on a command line) and
# that `kiro-cli whoami` succeeds in a throwaway HOME; the profile hands the key to the pane by
# `env:KIRO_API_KEY`. Devin and OpenCode need your own login (`devin auth status`,
# `opencode auth list`) and use your real HOME.
#
# Settings (environment): SHEPHERD_BIN, PORT (default 8769), R_HOME, R_WORK (both must contain
# shepherd-e2e-restart), KEEP=1 (leave everything behind), KIRO_API_KEY (kiro),
# CANARY_DEVIN_CWD (devin: a directory you trusted with Devin once, see README D0; the script
# uses it instead of a throwaway directory and removes only its marker file).
#
# `C_SOURCE_ONLY=1 source dialog-canary.sh` defines the functions and runs nothing; the
# repository's tests use it to check the files c_setup writes.
set -u
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
R_HOME="${R_HOME:-$HOME/.config/shepherd-e2e-restart-canary}"
R_WORK="${R_WORK:-/tmp/shepherd-e2e-restart-canary}"
PORT="${PORT:-8769}"
# shellcheck source=restart-lib.sh
source "$HERE/restart-lib.sh"

C_AGENT="${1:-}"
C_CLIENTS="$R_HOME/clients.json"
C_AUDIT_DIR="$R_HOME/audit"           # the server refuses to start with a clients file and no audit file
C_AUDIT="$C_AUDIT_DIR/audit.jsonl"
C_HOME="$R_WORK/home"                 # the throwaway HOME of the holder (kiro only)
C_PANES="$R_WORK/panes"
C_CWD="$C_PANES/canary"               # the pane's working directory (devin may use CANARY_DEVIN_CWD)
C_MARKER_NAME="canary_marker.txt"
C_SCREEN="$R_WORK/screen.txt"
C_PANE=canary
C_STARTED=0                           # 1 once start_pane succeeded (the cleanup removes the pane)
C_OWN_CWD=1                           # 0 when the cwd is the maintainer's (CANARY_DEVIN_CWD)
C_CLI=""
C_REASON=""
C_START_WAIT=90                       # seconds the first screen gets to settle
C_PROMPT_WAIT=150                     # seconds the agent gets to ask for permission
C_STRUCT_WAIT=10                      # seconds a partly drawn dialog gets to finish
C_SETTLE_WAIT=60                      # seconds the agent gets to settle after the answer
C_WINDOW=3                            # seconds a "nothing happened" check watches
C_ROWS=30                             # screen lines printed in a drift report
export SHEPHERD_TOKEN_FILE="$R_HOME/canary.token"   # r_mcp_up and the MCP helpers use it

# The fixture each agent is compared with. Devin has none: the check holds the layout that
# src/devin_dialog.rs defines.
c_fixture() {
  case "$1" in
    kiro) echo "$HERE/../../tests/fixtures/replay/kiro/03-permission-dialog.screen.txt" ;;
    opencode) echo "$HERE/../../tests/fixtures/replay/opencode/03-permission-dialog.screen.txt" ;;
    *) echo "" ;;
  esac
}

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

# c_py EXPRESSION : evaluate a Python expression on the JSON from stdin (`d`); "null" for None,
# nothing at all when the input is not JSON or the expression fails.
c_py() {
  python3 -c 'import json, sys
d = json.load(sys.stdin)
v = eval(sys.argv[1])
print("null" if v is None else (v if isinstance(v, str) else json.dumps(v)))' "$1" 2>/dev/null
}

# c_bounded SECONDS CMD... : run CMD with no stdin and a time limit; print its output; 124 on a timeout.
c_bounded() {
  python3 -c 'import subprocess, sys
try:
    p = subprocess.run(sys.argv[2:], stdin=subprocess.DEVNULL, capture_output=True,
                       text=True, errors="replace", timeout=float(sys.argv[1]))
except subprocess.TimeoutExpired:
    sys.exit(124)
except OSError:
    sys.exit(127)
sys.stdout.write(p.stdout + p.stderr)
sys.exit(p.returncode)' "$@"
}

# c_whoami SECONDS CLI HOME : `kiro-cli whoami` in a clean environment: only HOME, PATH, TERM, LANG and
# KIRO_API_KEY. The key is read from this process's environment by name; it is not in argv.
c_whoami() {
  KIRO_API_KEY="${KIRO_API_KEY:-}" PATH="$PATH" python3 -c 'import os, subprocess, sys
secs, cli, home = float(sys.argv[1]), sys.argv[2], sys.argv[3]
env = {"HOME": home, "PATH": os.environ["PATH"], "TERM": "xterm-256color", "LANG": "en_US.UTF-8",
       "KIRO_API_KEY": os.environ["KIRO_API_KEY"]}
try:
    p = subprocess.run([cli, "whoami"], stdin=subprocess.DEVNULL, capture_output=True, env=env, timeout=secs)
except (subprocess.TimeoutExpired, OSError):
    sys.exit(1)
sys.exit(p.returncode)' "$1" "$2" "$3"
}

# c_pin AGENT -> the version the Dockerfile pins (empty when it pins none). OpenCode is not in
# the image: print the version its dialect module was measured on.
c_pin() {
  local dockerfile="$HERE/../../Dockerfile"
  case "$1" in
    kiro) sed -n 's/^ARG KIRO_CLI_VERSION=\(.*\)$/\1/p' "$dockerfile" 2>/dev/null | head -1 ;;
    devin) sed -n 's/^ARG DEVIN_CLI_VERSION=\(.*\)$/\1/p' "$dockerfile" 2>/dev/null | head -1 ;;
    opencode) echo "2.2.7" ;;
  esac
}

# c_version_matches PIN VERSION_TEXT : the pin appears in the text as a whole version number.
c_version_matches() {
  [ -n "$1" ] && printf '%s' "$2" | grep -Eq "(^|[^0-9.])$(printf '%s' "$1" | sed 's/\./\\./g')([^0-9.]|$)"
}

# c_version_notice AGENT CLI : print the installed version against the pin. Never a failure.
c_version_notice() {
  local pin text
  pin="$(c_pin "$1")"
  text="$(c_bounded 20 "$2" --version 2>&1 | head -3 | tr '\n' ' ')"
  C_VERSION="${text:-unknown}"
  echo "canary: $1 --version: $C_VERSION"
  if [ -z "$pin" ]; then
    echo "canary: NOTICE the Dockerfile pins no $1 version; the result describes the installed one"
  elif c_version_matches "$pin" "$text"; then
    echo "canary: $1 matches the pin ($pin)"
  else
    echo "canary: NOTICE the installed $1 differs from the pin $pin: the result describes the NEW version"
  fi
}
C_VERSION=unknown
C_OUTCOME=""

# c_prereq AGENT : set C_CLI. Return 1 with C_REASON when the agent cannot authenticate or is not installed.
c_prereq() {
  local out
  C_REASON=""
  case "$1" in
    kiro)
      C_CLI="$(command -v kiro-cli || true)"
      case "$C_CLI" in /*) ;; *) C_REASON="kiro-cli is not on PATH as an absolute path"; return 1 ;; esac
      if [ -z "${KIRO_API_KEY:-}" ]; then
        C_REASON="KIRO_API_KEY is not set, so kiro-cli would ask for a login (this script never logs in)"
        return 1
      fi
      local scratch rc
      scratch="$(mktemp -d "${TMPDIR:-/tmp}/canary-whoami.XXXXXX")" || return 1
      c_whoami 30 "$C_CLI" "$scratch"; rc=$?
      rm -rf "$scratch"
      if [ "$rc" -ne 0 ]; then
        C_REASON="kiro-cli whoami failed with the key in a throwaway HOME: the key is not accepted"
        return 1
      fi
      ;;
    devin)
      C_CLI="$(command -v devin || true)"
      case "$C_CLI" in /*) ;; *) C_REASON="devin is not on PATH as an absolute path"; return 1 ;; esac
      out="$(c_bounded 30 "$C_CLI" auth status 2>&1)"
      if ! printf '%s' "$out" | grep -qi 'logged in' || printf '%s' "$out" | grep -qi 'not logged in'; then
        C_REASON="devin is not logged in (run: devin auth login yourself; this script never does)"
        return 1
      fi
      ;;
    opencode)
      C_CLI="$(command -v opencode || true)"
      case "$C_CLI" in /*) ;; *) C_REASON="opencode is not on PATH as an absolute path"; return 1 ;; esac
      out="$(c_bounded 30 "$C_CLI" auth list 2>&1)"
      if [ -z "$out" ] || printf '%s' "$out" | grep -Eqi '(^|[^0-9])0 (credentials|providers)'; then
        C_REASON="opencode has no provider login (run: opencode auth login yourself; this script never does)"
        return 1
      fi
      ;;
  esac
  return 0
}

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

# c_write_profile AGENT CLI CWD OUT : the one launch profile. Everything it needs is an argument.
c_write_profile() {
  python3 - "$1" "$2" "$3" "$4" <<'PY'
import json, os, sys

agent, cli, cwd, out = sys.argv[1:]
profile = {"name": "canary", "agent": agent, "cwds": [cwd]}
if agent == "kiro":
    # A workspace agent `probe` (written in the cwd) trusts no tool, so the shell prompt ends on
    # an approval dialog. The key reaches the pane only through this reference.
    profile["argv"] = [cli, "chat", "--agent", "probe", "--trust-workspace"]
    profile["env"] = {"KIRO_API_KEY": "env:KIRO_API_KEY"}
elif agent == "devin":
    profile["argv"] = [cli]
    profile["permission_mode"] = "normal"
else:
    profile["argv"] = [cli, "--standalone"]
    profile["permission_mode"] = "normal"
with open(out, "w") as f:
    json.dump({"allow_arbitrary_command": False, "profiles": [profile]}, f, indent=2)
    f.write("\n")
os.chmod(out, 0o600)
PY
}

# c_write_clients OUT HASH : the clients file; the client may only look, read, control, start,
# answer dialogs and remove, on the one profile.
c_write_clients() {
  python3 - "$1" "$2" <<'PY'
import json, os, sys

out, token_hash = sys.argv[1:]
doc = {"version": 1, "clients": [
    {"id": "canary", "token_sha256": token_hash,
     "scopes": ["observe", "read", "control", "start", "dialog", "remove"],
     "profiles": ["canary"]},
]}
with open(out, "w") as f:
    json.dump(doc, f, indent=2)
    f.write("\n")
os.chmod(out, 0o600)
PY
}

# c_setup AGENT CLI : token, clients file, profile, directories. Safe to run again.
c_setup() {
  local agent="$1" cli="$2"
  [ -x "$BIN" ] || r_die "no executable at $BIN (build it on the build host and copy it, see restart-survival.md)"
  for tool in python3 openssl curl pgrep; do
    command -v "$tool" >/dev/null || r_die "$tool is not installed"
  done
  case "$R_WORK" in */shepherd-e2e-restart*) ;; *) r_die "refusing to use R_WORK=$R_WORK (the path must contain shepherd-e2e-restart)" ;; esac
  case "$R_HOME" in */shepherd-e2e-restart*) ;; *) r_die "refusing to use R_HOME=$R_HOME (the path must contain shepherd-e2e-restart)" ;; esac
  if git -C "$R_HOME" rev-parse --show-toplevel >/dev/null 2>&1; then
    r_die "$R_HOME is inside a git repository; the token and the clients file must be outside every repository"
  fi
  if [ "$agent" = devin ] && [ -n "${CANARY_DEVIN_CWD:-}" ]; then
    [ -d "$CANARY_DEVIN_CWD" ] || r_die "CANARY_DEVIN_CWD=$CANARY_DEVIN_CWD is not a directory"
    case "$CANARY_DEVIN_CWD" in /*) ;; *) r_die "CANARY_DEVIN_CWD must be an absolute path" ;; esac
    C_CWD="$(cd "$CANARY_DEVIN_CWD" && pwd -P)"
    C_OWN_CWD=0
  fi
  (
    umask 077
    mkdir -p "$R_HOME" "$R_WORK" "$C_AUDIT_DIR"
    chmod 700 "$R_HOME" "$R_WORK" "$C_AUDIT_DIR"
    rm -f "$C_AUDIT"
    rm -rf "${R_WORK:?}/server" "${C_PANES:?}" "${C_HOME:?}"
    mkdir -p "$R_WORK/server" "$C_PANES" "$C_HOME"
    [ "$C_OWN_CWD" = 1 ] && mkdir -p "$C_CWD"
    rm -f "$C_CWD/$C_MARKER_NAME"
    [ -s "$R_HOME/canary.token" ] || printf 'osh_canary_%s' "$(openssl rand -hex 32)" > "$R_HOME/canary.token"
    chmod 600 "$R_HOME/canary.token"
    c_write_clients "$C_CLIENTS" "$(openssl dgst -sha256 < "$R_HOME/canary.token" | awk '{print $NF}')"
    if [ "$agent" = kiro ]; then
      mkdir -p "$C_CWD/.kiro/agents"
      cat > "$C_CWD/.kiro/agents/probe.json" <<'JSON'
{
  "name": "probe",
  "description": "Canary agent that trusts no tool",
  "tools": ["@builtin"],
  "allowedTools": []
}
JSON
    fi
    if [ "$agent" = opencode ]; then
      # Without an ask rule OpenCode runs the command with no dialog and there is nothing to answer.
      cat > "$C_CWD/opencode.jsonc" <<'JSON'
{
  "$schema": "https://opencode.ai/config.json",
  "permissions": [
    { "action": "shell", "resource": "*", "effect": "ask" },
    { "action": "edit", "resource": "*", "effect": "ask" }
  ]
}
JSON
    fi
  ) || r_die "setup failed"
  C_CWD="$(cd "$C_CWD" && pwd -P)"
  c_write_profile "$agent" "$cli" "$C_CWD" "$R_HOME/profiles.json" || r_die "writing the profile failed"
  : > "$R_VERSION_FILE"
  r_say "setup done: token, clients and profile in $R_HOME, pane directory $C_CWD"
}

# c_prompt AGENT : the one prompt.
c_prompt() {
  case "$1" in
    kiro) printf 'Use the shell tool to run exactly this command in the current directory and nothing else: touch %s' "$C_MARKER_NAME" ;;
    *) printf 'Use the shell tool to run exactly: touch %s/%s . Do nothing else.' "$C_CWD" "$C_MARKER_NAME" ;;
  esac
}

# c_start_holder AGENT : `openab-shepherd hold` with the clients file and the audit file, in the background; waits for MCP.
# Kiro: the holder (and so the pane) gets the throwaway HOME and the key. The others get your real
# HOME (their login lives there) and no Kiro key.
c_start_holder() {
  if r_port_in_use "$PORT"; then r_die "port $PORT is in use; set PORT=<another port>"; fi
  : > "$R_LOG"
  local home="$HOME"
  [ "$1" = kiro ] && home="$C_HOME"
  (
    cd "$R_WORK/server" || exit 1
    unset_key=()
    [ "$1" = kiro ] || unset_key=(-u KIRO_API_KEY)
    exec env -u OPENAB_SHEPHERD_BACKEND -u OPENAB_SHEPHERD_TOKEN -u OPENAB_SHEPHERD_TOKEN_FILE \
      ${unset_key[@]+"${unset_key[@]}"} \
      HOME="$home" \
      OPENAB_SHEPHERD_CLIENTS="$C_CLIENTS" \
      OPENAB_SHEPHERD_AUDIT_FILE="$C_AUDIT" \
      OPENAB_SHEPHERD_PROFILES="$R_HOME/profiles.json" \
      OPENAB_SHEPHERD_TEST_HOLDER_VERSION="@$R_VERSION_FILE" \
      "$BIN" hold -- --host 127.0.0.1 --port "$PORT"
  ) >> "$R_LOG" 2>&1 < /dev/null &
  echo $! > "$R_PIDFILE"
  r_wait_mcp 30 || { tail -20 "$R_LOG" >&2; r_die "the server did not come up within 30 s"; }
  r_say "holder pid $(r_holder_pid), port $PORT, log $R_LOG"
}

# c_cleanup : remove the pane, then the holder and the throwaway files (r_cleanup; not with KEEP=1).
c_cleanup() {
  if [ "${KEEP:-}" != 1 ]; then
    if [ "$C_STARTED" = 1 ]; then remove_pane "$C_PANE" >/dev/null 2>&1 || true; fi
    [ "$C_OWN_CWD" = 1 ] || rm -f "$C_CWD/$C_MARKER_NAME"
  fi
  r_cleanup
}

# ---------------------------------------------------------------------------
# Steps
# ---------------------------------------------------------------------------

# c_wait_state SECONDS : poll until the pane is idle or blocked; print the state. Bounded.
c_wait_state() {
  local end=$((SECONDS + $1)) state=""
  while :; do
    state="$(r_field "$C_PANE" status.state)"
    case "$state" in idle|blocked) echo "$state"; return 0 ;; esac
    [ "$SECONDS" -ge "$end" ] && { echo "$state"; return 1; }
    sleep 1
  done
}

# c_wait_permission SECONDS : wait_status until the pane is blocked with kind permission. Bounded; the
# last outcome is left in C_OUTCOME.
c_wait_permission() {
  local end=$((SECONDS + $1)) out kind
  C_OUTCOME=""
  while :; do
    out="$(wait_status "$C_PANE" 20 2>/dev/null)"
    C_OUTCOME="$(printf '%s' "$out" | c_py 'd.get("outcome")')"
    kind="$(printf '%s' "$out" | c_py 'd.get("kind")')"
    [ "$C_OUTCOME" = blocked ] && [ "$kind" = permission ] && return 0
    case "$C_OUTCOME" in done|cancelled) return 1 ;; esac
    [ "$SECONDS" -ge "$end" ] && return 1
    sleep 1
  done
}

# c_screen : save the pane's screen text to $C_SCREEN.
c_screen() {
  read_screen "$C_PANE" > "$C_SCREEN" 2>/dev/null && [ -s "$C_SCREEN" ]
}

# c_report SEEN_FILE : the drift report: the version and the end of what the pane showed.
c_report() {
  echo
  echo "---- drift report ($C_AGENT) ----"
  echo "installed: $C_VERSION"
  echo "pinned:    $(c_pin "$C_AGENT")"
  echo "seen (last $C_ROWS lines; read it before you paste it anywhere):"
  tail -n "$C_ROWS" "$1" | sed 's/^/  | /'
  echo "---- end of report ----"
  echo
}

# c_marker_absent : the marker file does not exist.
c_marker_absent() { [ ! -e "$C_CWD/$C_MARKER_NAME" ]; }

# c_answered RC CHOICE : answer_dialog succeeded and reports the choice that was asked for.
c_answered() { [ "$1" -eq 0 ] && [ "$2" = deny_once ]; }

# c_stays SECONDS CMD... : CMD must succeed now and at every half second of the window.
c_stays() {
  local end=$((SECONDS + $1))
  shift
  while :; do
    "$@" || return 1
    [ "$SECONDS" -ge "$end" ] && return 0
    sleep 0.5
  done
}

# c_finish : the summary. Like r_finish, but a run that checked nothing, or skipped anything, is not a
# PASS: it prints RESULT: INCOMPLETE and exits 3. Only a run with passed > 0, no skip and no failure
# is evidence for issue #65.
c_finish() {
  if [ "$R_FAILS" -ne 0 ]; then r_finish; return $?; fi
  if [ "$R_PASSES" -gt 0 ] && [ "$R_SKIPS" -eq 0 ]; then r_finish; return $?; fi
  echo
  echo "host:    $(uname -srm)"
  echo "summary: $R_PASSES passed, $R_FAILS failed, $R_SKIPS skipped"
  echo "RESULT: INCOMPLETE (a check was skipped or nothing was checked; this is not a pass and not evidence for #65)"
  return 3
}

c_usage() { echo "usage: bash dialog-canary.sh kiro|devin|opencode" >&2; exit 2; }

c_main() {
  case "$C_AGENT" in kiro|devin|opencode) ;; *) c_usage ;; esac
  local out rc state fixture seen_file
  fixture="$(c_fixture "$C_AGENT")"
  [ -x "$BIN" ] || r_die "no executable at $BIN (build it on the build host and copy it, see restart-survival.md)"

  # --- Step 0: the agent can authenticate -------------------------------------------------
  if ! c_prereq "$C_AGENT"; then
    r_skip "$C_AGENT: $C_REASON"
    echo "canary: nothing was checked and no model prompt was sent"
    c_finish
    exit $?
  fi
  c_version_notice "$C_AGENT" "$C_CLI"

  # --- Step 1: setup, holder --------------------------------------------------------------
  c_setup "$C_AGENT" "$C_CLI"
  trap c_cleanup EXIT
  trap 'exit 130' INT TERM
  c_start_holder "$C_AGENT"

  # --- Step 2: start the pane, look at the first screen -----------------------------------
  out="$(start_pane canary "$C_CWD" "$C_PANE" 2>/dev/null)"; rc=$?
  if [ "$rc" -ne 0 ]; then
    if [ "$(printf '%s' "$out" | c_py 'd.get("error")')" = trust_prompt ]; then
      r_skip "$C_AGENT: asks to trust the directory (do the one-time trust of README D0 and set CANARY_DEVIN_CWD to that directory)"
      c_finish
      exit $?
    fi
    r_check "start_pane starts the $C_AGENT pane" false
    echo "$out" | head -20
    c_finish
    exit $?
  fi
  C_STARTED=1
  state="$(c_wait_state "$C_START_WAIT")"
  c_screen || : > "$C_SCREEN"
  if python3 "$HERE/dialog-canary-check.py" login "$C_SCREEN" >/dev/null \
     || [ "$(r_field "$C_PANE" status.kind)" = auth ]; then
    remove_pane "$C_PANE" >/dev/null 2>&1; C_STARTED=0
    r_skip "$C_AGENT: the first screen asks for a login; the pane was removed and nothing was typed (log in yourself, this script never does)"
    c_finish
    exit $?
  fi
  r_check "the $C_AGENT pane reaches idle (state: ${state:-none})" test "$state" = idle
  if [ "$state" != idle ]; then c_report "$C_SCREEN"; c_finish; exit $?; fi

  # --- Step 3: the one prompt -------------------------------------------------------------
  out="$(send_input "$C_PANE" "$(c_prompt "$C_AGENT")" 2>&1)"
  r_check "send_input delivers the prompt" test "$(printf '%s' "$out" | c_py 'd.get("started")')" = true
  r_check "the agent asks for permission (blocked, kind permission)" c_wait_permission "$C_PROMPT_WAIT"
  if [ "$C_OUTCOME" != blocked ]; then
    echo "canary: the last outcome was '${C_OUTCOME:-none}'; the agent did not ask (it may have run the command, or refused)"
    c_screen || : > "$C_SCREEN"
    c_report "$C_SCREEN"
    c_finish
    exit $?
  fi

  # --- Step 4: the structure of the dialog ------------------------------------------------
  local fixture_args=() sdeadline=$((SECONDS + C_STRUCT_WAIT))
  [ -n "$fixture" ] && fixture_args=(--fixture "$fixture")
  # The dialog may still be drawing when `blocked` is reported: read and compare again, bounded.
  while :; do
    c_screen || : > "$C_SCREEN"
    out="$(python3 "$HERE/dialog-canary-check.py" structure "$C_AGENT" "$C_SCREEN" --marker "$C_MARKER_NAME" ${fixture_args[@]+"${fixture_args[@]}"} 2>&1)"; rc=$?
    [ "$rc" -eq 0 ] && break
    [ "$SECONDS" -ge "$sdeadline" ] && break
    sleep 1
  done
  r_check "the screen can be read" test -s "$C_SCREEN"
  echo "canary: $out"
  r_check "the dialog has the recorded structure (title, tool line, options in order)" test "$rc" -eq 0
  [ "$rc" -eq 0 ] || c_report "$C_SCREEN"

  # --- Step 5: deny_once through the production parser ------------------------------------
  c_marker_absent || r_check "the marker does not exist before the answer" false
  out="$(answer_dialog "$C_PANE" deny_once 2>/dev/null)"; rc=$?
  r_check "answer_dialog deny_once succeeds (the shipped parser reads the dialog)" \
    c_answered "$rc" "$(printf '%s' "$out" | c_py 'd.get("choice")')"
  if [ "$rc" -ne 0 ]; then
    echo "canary: answer_dialog said: $(printf '%s' "$out" | c_py 'd.get("reason")')"
    seen_file="$R_WORK/seen.txt"
    printf '%s' "$out" | python3 -c 'import json, sys
try:
    d = json.load(sys.stdin)
    seen = d.get("seen")
    print(seen.get("screen", "") if isinstance(seen, dict) else (seen or ""))
except Exception:
    pass' > "$seen_file" 2>/dev/null
    [ -s "$seen_file" ] || cp "$C_SCREEN" "$seen_file"
    c_report "$seen_file"
  else
    # The agent may go on (a retry, another tool); wait for it to settle, then look at the file.
    c_wait_state "$C_SETTLE_WAIT" >/dev/null
    r_check "the marker file does not exist after deny_once" c_stays "$C_WINDOW" c_marker_absent
  fi

  # --- Step 6: remove the pane ------------------------------------------------------------
  out="$(remove_pane "$C_PANE" 2>&1)"; rc=$?
  r_check "remove_pane removes the pane" test "$rc" -eq 0
  [ "$rc" -eq 0 ] && C_STARTED=0
  c_finish
}

if [ "${C_SOURCE_ONLY:-}" != 1 ]; then c_main; fi
